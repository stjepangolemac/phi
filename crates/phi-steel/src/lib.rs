use std::{fs, path::Path};

use anyhow::{Context, Result};
use phi_protocol::{CommandSpec, Event, ModelSpec, PolicyOutput};
use steel::{rvals::FromSteelVal, steel_vm::engine::Engine};

pub struct Policy {
    vm: Engine,
    state: String,
}

const PLUGIN_PRELUDE: &str = r#"
(define command-registry '())
(define model-registry '())
(define tool-registry '())
(define agent-instructions "")
(define session-id "")

(define (configure-runtime! encoded-config)
  (define config (string->jsexpr encoded-config))
  (set! tool-registry (or (hash-try-get config 'tools) '()))
  (set! session-id (or (hash-try-get config 'session_id) "")))

(define (set-agent-instructions! value)
  (set! agent-instructions value))

(define (register-command! spec handler)
  (set! command-registry
        (append command-registry (list (hash 'spec spec 'handler handler)))))

(define (register-model! spec)
  (set! model-registry (append model-registry (list spec))))

(define (registered-command-specs)
  (map (lambda (entry) (hash-ref entry 'spec)) command-registry))

(define (registered-models) model-registry)
(define (registered-tools) tool-registry)
(define (runtime-session-id) session-id)

(define (default-model-id)
  (define (find-default models)
    (cond [(null? models) (error! "provider registered no default model")]
          [(hash-ref (car models) 'default) (hash-ref (car models) 'id)]
          [else (find-default (cdr models))]))
  (find-default model-registry))

(define (default-model-spec)
  (define (find-default models)
    (cond [(null? models) (error! "provider registered no default model")]
          [(hash-ref (car models) 'default) (car models)]
          [else (find-default (cdr models))]))
  (find-default model-registry))

(define (default-model-reasoning)
  (hash-ref (default-model-spec) 'default_reasoning))

(define (default-model-service-tier)
  (hash-ref (default-model-spec) 'default_service_tier))

(define (dispatch-command name state arguments)
  (define (find entries)
    (cond [(null? entries) (error! "unknown plugin command")]
          [(equal? name (hash-ref (hash-ref (car entries) 'spec) 'name))
           ((hash-ref (car entries) 'handler) state arguments)]
          [else (find (cdr entries))]))
  (find command-registry))
"#;

impl Policy {
    pub fn load(agent: &Path, provider: &Path, prompt: &Path, compaction: &Path) -> Result<Self> {
        Self::load_with_state(
            agent,
            provider,
            prompt,
            compaction,
            r#"{"context_char_budget":24000}"#,
            None,
        )
    }

    pub fn load_with_state(
        agent: &Path,
        provider: &Path,
        prompt: &Path,
        compaction: &Path,
        config: &str,
        state: Option<String>,
    ) -> Result<Self> {
        let mut vm = Engine::new_sandboxed();
        let source = format!(
            "{}\n{}\n{}\n{}\n{}",
            PLUGIN_PRELUDE,
            fs::read_to_string(provider)?,
            fs::read_to_string(compaction)?,
            fs::read_to_string(prompt)?,
            fs::read_to_string(agent)?
        );
        vm.compile_and_run_raw_program(source)
            .context("load Steel policy")?;
        eval_string(
            &mut vm,
            &format!(
                "(begin (configure-runtime! {}) \"\")",
                scheme_string(config)
            ),
        )?;
        let state = match state {
            Some(state) => state,
            None => eval_string(&mut vm, &format!("(init {})", scheme_string(config)))?,
        };
        Ok(Self { vm, state })
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn on_event(&mut self, event: &Event) -> Result<PolicyOutput> {
        let event = serde_json::to_string(event)?;
        let expression = format!(
            "(on-event {} {})",
            scheme_string(&self.state),
            scheme_string(&event)
        );
        let encoded = eval_string(&mut self.vm, &expression)?;
        let output: PolicyOutput =
            serde_json::from_str(&encoded).context("decode policy output")?;
        self.state.clone_from(&output.state);
        Ok(output)
    }

    pub fn commands(&mut self) -> Result<Vec<CommandSpec>> {
        let encoded = eval_string(
            &mut self.vm,
            "(value->jsexpr-string (registered-command-specs))",
        )?;
        serde_json::from_str(&encoded).context("decode plugin commands")
    }

    pub fn models(&mut self) -> Result<Vec<ModelSpec>> {
        let encoded = eval_string(&mut self.vm, "(value->jsexpr-string (registered-models))")?;
        serde_json::from_str(&encoded).context("decode provider models")
    }

    pub fn run_command(&mut self, name: &str, arguments: &str) -> Result<String> {
        let expression = format!(
            "(value->jsexpr-string (dispatch-command {} (string->jsexpr {}) {}))",
            scheme_string(name),
            scheme_string(&self.state),
            scheme_string(arguments),
        );
        let encoded = eval_string(&mut self.vm, &expression)?;
        let output: PluginCommandOutput =
            serde_json::from_str(&encoded).context("decode plugin command output")?;
        self.state = serde_json::to_string(&output.state)?;
        Ok(output.content)
    }
}

#[derive(serde::Deserialize)]
struct PluginCommandOutput {
    state: serde_json::Value,
    content: String,
}

fn eval_string(vm: &mut Engine, expression: &str) -> Result<String> {
    let values = vm
        .compile_and_run_raw_program(expression.to_owned())
        .context("run Steel policy")?;
    let value = values.last().context("Steel policy returned no value")?;
    String::from_steelval(value).map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn scheme_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

pub fn check(agent: &Path, provider: &Path, prompt: &Path, compaction: &Path) -> Result<()> {
    let _ = Policy::load(agent, provider, prompt, compaction)?;
    Ok(())
}

pub fn replay_smoke(agent: &Path, provider: &Path, prompt: &Path, compaction: &Path) -> Result<()> {
    let mut policy = Policy::load(agent, provider, prompt, compaction)?;
    let output = policy.on_event(&Event::UserMessage {
        content: "replay fixture".into(),
    })?;
    if output.effects.is_empty() {
        anyhow::bail!("candidate replay emitted no effects");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_protocol::Effect;

    #[test]
    fn policy_maps_events_to_provider_and_finish_effects() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        let output = policy
            .on_event(&Event::UserMessage {
                content: "hello".into(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { url, body, .. }
                if url.ends_with("/responses")
                    && body["model"] == "gpt-5.6-luna"
                    && body["reasoning"]["effort"] == "low"
                    && body["prompt_cache_key"] == ""
                    && body.get("service_tier").is_none()
        ));

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![
                    serde_json::json!({
                        "type": "response.created",
                        "response": { "usage": null }
                    }),
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "world"
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": { "usage": { "total_tokens": 12 } }
                    }),
                ],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(
            output.effects,
            vec![Effect::Finish {
                content: "world".into()
            }]
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["last_usage"]["total_tokens"], 12.0);
    }

    #[test]
    fn preserves_openai_reasoning_items_and_assistant_phase() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        policy
            .on_event(&Event::UserMessage {
                content: "first".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "reasoning",
                            "id": "reasoning-1",
                            "encrypted_content": "opaque"
                        }
                    }),
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "compaction",
                            "encrypted_content": "compact"
                        }
                    }),
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "message",
                            "role": "assistant",
                            "phase": "final_answer",
                            "content": []
                        }
                    }),
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "answer"
                    }),
                ],
                error: String::new(),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::UserMessage {
                content: "second".into(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["input"][1]["type"] == "reasoning"
                    && body["input"][1]["encrypted_content"] == "opaque"
                    && body["input"][2]["type"] == "compaction"
                    && body["input"][2]["encrypted_content"] == "compact"
                    && body["input"][3]["phase"] == "final_answer"
                    && body["input"][3]["content"][0]["text"] == "answer"
        ));
    }

    #[test]
    fn prompt_plugin_controls_provider_neutral_prompt() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let prompt = temp.path().join("prompt.scm");
        fs::write(
            &prompt,
            r#"(define (build-prompt messages instructions tools)
                 (hash 'instructions "custom instructions"
                       'messages (list (hash 'kind "message" 'role "user"
                                             'content "custom message"))
                       'tools '()))"#,
        )
        .unwrap();
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &prompt,
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        let output = policy
            .on_event(&Event::UserMessage {
                content: "ignored by prompt".into(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["tools"].as_array().unwrap().is_empty()
                    && body["instructions"] == "custom instructions"
                    && body["input"][0]["content"][0]["text"] == "custom message"
        ));
    }

    #[test]
    fn policy_continues_after_a_tool_result() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        policy
            .on_event(&Event::UserMessage {
                content: "inspect".into(),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "read_file",
                        "arguments": "{\"path\":\"policy/agent.scm\"}"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::RunTool { name, .. } if name == "read_file"));
        let output = policy
            .on_event(&Event::ToolCompleted {
                name: "read_file".into(),
                result: serde_json::json!({ "content": "policy" }),
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::HttpRequest { .. }));
    }

    #[test]
    fn compaction_bounds_large_context() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load_with_state(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
            r#"{"context_char_budget":4000}"#,
            None,
        )
        .unwrap();
        for _ in 0..10 {
            policy
                .on_event(&Event::UserMessage {
                    content: "x".repeat(3_000),
                })
                .unwrap();
        }
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert!(state["compactions"].as_f64().unwrap() > 0.0);
        assert!(serde_json::to_string(&state["messages"]).unwrap().len() <= 4_000);
        assert!(state["estimated_tokens"].as_f64().unwrap() <= 1_000.0);
    }

    #[test]
    fn compaction_truncates_oversized_tool_result() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load_with_state(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
            r#"{"context_char_budget":4000}"#,
            None,
        )
        .unwrap();
        policy
            .on_event(&Event::UserMessage {
                content: "list files".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "shell",
                        "arguments": "{}"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        policy
            .on_event(&Event::ToolCompleted {
                name: "shell".into(),
                result: serde_json::json!({ "stdout": "x".repeat(64 * 1024) }),
            })
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert!(serde_json::to_string(&state["messages"]).unwrap().len() <= 4_000);
        let messages = state["messages"].as_array().unwrap();
        assert_eq!(messages[messages.len() - 2]["kind"], "tool_call");
        assert_eq!(messages[messages.len() - 1]["kind"], "tool_result");
    }

    #[test]
    fn provider_registers_models_and_model_selection_persists() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        assert_eq!(policy.models().unwrap()[0].id, "gpt-5.6-luna");
        let output = policy
            .on_event(&Event::ModelSelected {
                model: "gpt-5.6-luna".into(),
                reasoning: "low".into(),
                service_tier: "default".into(),
            })
            .unwrap();
        assert!(
            matches!(&output.effects[0], Effect::Finish { content } if content.contains("gpt-5.6-luna"))
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["model"], "gpt-5.6-luna");
        assert_eq!(state["reasoning"], "low");
        assert_eq!(state["service_tier"], "default");
    }

    #[test]
    fn plugin_can_register_and_run_command() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("provider.scm");
        fs::write(
            &provider,
            format!(
                "{}\n{}",
                fs::read_to_string(root.join("policy/providers/openai.scm")).unwrap(),
                r#"(register-command!
                      (hash 'name "echo" 'usage "/echo TEXT"
                            'description "Echo text." 'source "test")
                      (lambda (state arguments)
                        (hash 'state state 'content arguments)))"#
            ),
        )
        .unwrap();
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &provider,
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        assert_eq!(policy.commands().unwrap()[0].name, "echo");
        assert_eq!(policy.run_command("echo", "hello").unwrap(), "hello");
    }

    #[test]
    fn malformed_tool_arguments_become_a_tool_error_input() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
            &root.join("policy/prompts/simple.scm"),
            &root.join("policy/compaction/simple.scm"),
        )
        .unwrap();
        policy
            .on_event(&Event::UserMessage {
                content: "inspect".into(),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "read_file",
                        "arguments": "not json"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::RunTool { arguments, .. } if arguments.get("malformed_arguments").is_some()
        ));
    }
}
