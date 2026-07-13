use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use phi_protocol::{CommandSpec, Event, ModelSpec, PolicyOutput};
use steel::{rvals::FromSteelVal, steel_vm::engine::Engine};

pub struct Policy {
    vm: Engine,
    state: String,
}

const PLUGIN_PRELUDE: &str = r#"
(require-builtin steel/json)
(require-builtin steel/hash)

(define command-registry '())
(define model-registry '())
(define tool-registry '())
(define tool-implementation-registry '())
(define tool-selection-registry '())
(define tool-config-registry '())
(define provider-registry '())
(define prompt-builder-registry '())
(define compactor-registry '())
(define selected-prompt-builder "")
(define selected-compactor "")
(define selected-compactor-config (hash))
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

(define (register-provider! name effect call arguments output usage preserved phase)
  (set! provider-registry
        (append provider-registry
                (list (hash 'name name 'effect effect 'call call
                            'arguments arguments 'output output 'usage usage
                            'preserved preserved 'phase phase)))))

(define (register-model! provider spec)
  (define model (hash-ref spec 'id))
  (set! model-registry
        (append model-registry
                (list (hash-insert
                        (hash-insert
                          (hash-insert spec 'provider provider)
                          'model model)
                        'id (string-append provider "/" model))))))

(define (register-hosted-tool! name capability provider build)
  (set! tool-implementation-registry
        (append tool-implementation-registry
                (list (hash 'name name 'capability capability 'mode "hosted"
                            'provider provider 'build build)))))

(define (register-callable-tool! name capability spec start complete)
  (set! tool-implementation-registry
        (append tool-implementation-registry
                (list (hash 'name name 'capability capability 'mode "callable"
                            'spec spec 'start start 'complete complete)))))

(define (configure-tool! name config)
  (set! tool-config-registry
        (append tool-config-registry (list (hash 'name name 'config config)))))

(define (select-tool! capability preferences)
  (set! tool-selection-registry
        (append tool-selection-registry
                (list (hash 'name capability 'preferences preferences)))))

(define (register-prompt-builder! name builder)
  (set! prompt-builder-registry
        (append prompt-builder-registry (list (hash 'name name 'builder builder)))))

(define (register-compactor! name needed start complete)
  (set! compactor-registry
        (append compactor-registry
                (list (hash 'name name 'needed needed 'start start
                            'complete complete)))))

(define (select-prompt-builder! name) (set! selected-prompt-builder name))
(define (select-compactor! name config)
  (set! selected-compactor name)
  (set! selected-compactor-config config))
(define (load-plugin! _) #t)

(define (registered-command-specs)
  (map (lambda (entry) (hash-ref entry 'spec)) command-registry))

(define (registered-models) model-registry)
(define (registered-tools) tool-registry)
(define (runtime-session-id) session-id)

(define (find-named entries name)
  (cond [(null? entries) (error! (string-append "component not found: " name))]
        [(equal? name (hash-ref (car entries) 'name)) (car entries)]
        [else (find-named (cdr entries) name)]))

(define (model-spec id)
  (define (find models)
    (cond [(null? models) (error! (string-append "model not found: " id))]
          [(equal? id (hash-ref (car models) 'id)) (car models)]
          [else (find (cdr models))]))
  (find model-registry))

(define (model-provider id)
  (find-named provider-registry (hash-ref (model-spec id) 'provider)))

(define (string-member? value values)
  (cond [(null? values) #f]
        [(equal? value (car values)) #t]
        [else (string-member? value (cdr values))]))

(define (tool-config name)
  (define (find entries)
    (cond [(null? entries) (hash)]
          [(equal? name (hash-ref (car entries) 'name))
           (hash-ref (car entries) 'config)]
          [else (find (cdr entries))]))
  (find tool-config-registry))

(define (tool-compatible? implementation model)
  (define spec (model-spec model))
  (cond
    [(equal? (hash-ref implementation 'mode) "hosted")
     (and (equal? (hash-ref implementation 'provider)
                  (hash-ref spec 'provider))
          (string-member?
            (hash-ref implementation 'name)
            (or (hash-try-get spec 'hosted_tools) '())))]
    [(equal? (hash-ref implementation 'mode) "callable")
     (or (hash-try-get spec 'function_tools) #f)]
    [else #f]))

(define (find-compatible-hosted capability model implementations)
  (cond
    [(null? implementations) #f]
    [(and (equal? capability (hash-ref (car implementations) 'capability))
          (equal? (hash-ref (car implementations) 'mode) "hosted")
          (tool-compatible? (car implementations) model))
     (car implementations)]
    [else (find-compatible-hosted capability model (cdr implementations))]))

(define (resolve-tool-preference capability model preference)
  (define preferred (hash-try-get preference 'prefer))
  (define selected (hash-try-get preference 'use))
  (cond
    [(and preferred (equal? preferred "same-route-hosted"))
     (find-compatible-hosted capability model tool-implementation-registry)]
    [selected
     (define implementation
       (find-named tool-implementation-registry selected))
     (if (and (equal? capability (hash-ref implementation 'capability))
              (tool-compatible? implementation model))
         implementation
         #f)]
    [else (error! "invalid tool preference")]))

(define (resolve-tool-selection selection model)
  (define capability (hash-ref selection 'name))
  (define (resolve preferences)
    (cond
      [(null? preferences) #f]
      [else
       (define implementation
         (resolve-tool-preference capability model (car preferences)))
       (if implementation implementation (resolve (cdr preferences)))]))
  (resolve (hash-ref selection 'preferences)))

(define (resolved-tool-implementations model)
  (define (resolve selections)
    (cond
      [(null? selections) '()]
      [else
       (define implementation (resolve-tool-selection (car selections) model))
       (if implementation
           (cons implementation (resolve (cdr selections)))
           (resolve (cdr selections)))]))
  (resolve tool-selection-registry))

(define (resolved-tool-names model)
  (map (lambda (implementation) (hash-ref implementation 'name))
       (resolved-tool-implementations model)))

(define (resolved-tool-routes model)
  (map (lambda (implementation)
         (hash 'capability (hash-ref implementation 'capability)
               'implementation (hash-ref implementation 'name)))
       (resolved-tool-implementations model)))

(define (resolved-tool-spec implementation)
  (if (equal? (hash-ref implementation 'mode) "hosted")
      (hash 'kind "hosted_tool"
            'provider (hash-ref implementation 'provider)
            'implementation (hash-ref implementation 'name)
            'wire ((hash-ref implementation 'build)
                   (tool-config (hash-ref implementation 'name))))
      (hash-ref implementation 'spec)))

(define (tools-for-model model)
  (append tool-registry
          (map resolved-tool-spec (resolved-tool-implementations model))))

(define (callable-tool-for model name)
  (define (find implementations)
    (cond
      [(null? implementations) #f]
      [(and (equal? (hash-ref (car implementations) 'mode) "callable")
            (equal? (hash-ref (hash-ref (car implementations) 'spec) 'name)
                    name))
       (car implementations)]
      [else (find (cdr implementations))]))
  (find (resolved-tool-implementations model)))

(define (start-callable-tool implementation arguments)
  ((hash-ref implementation 'start)
   arguments (tool-config (hash-ref implementation 'name))))

(define (complete-callable-tool implementation events)
  ((hash-ref implementation 'complete)
   events (tool-config (hash-ref implementation 'name))))

(define (provider-request prompt model reasoning service-tier)
  ((hash-ref (model-provider model) 'effect)
   prompt (hash-ref (model-spec model) 'model) reasoning service-tier))
(define (provider-call-for model events)
  ((hash-ref (model-provider model) 'call) events))
(define (provider-arguments-for model call)
  ((hash-ref (model-provider model) 'arguments) call))
(define (provider-output-for model events)
  ((hash-ref (model-provider model) 'output) events))
(define (provider-usage-for model events)
  ((hash-ref (model-provider model) 'usage) events))
(define (provider-preserved-items-for model events)
  ((hash-ref (model-provider model) 'preserved) events))
(define (provider-message-phase-for model events)
  ((hash-ref (model-provider model) 'phase) events))

(define (build-selected-prompt messages instructions tools)
  ((hash-ref (find-named prompt-builder-registry selected-prompt-builder) 'builder)
   messages instructions tools))

(define (selected-compaction-needed? messages max-chars)
  ((hash-ref (find-named compactor-registry selected-compactor) 'needed)
   messages max-chars selected-compactor-config))

(define (start-selected-compaction messages max-chars)
  ((hash-ref (find-named compactor-registry selected-compactor) 'start)
   messages max-chars selected-compactor-config))

(define (complete-selected-compaction messages max-chars events)
  ((hash-ref (find-named compactor-registry selected-compactor) 'complete)
   messages max-chars events selected-compactor-config))

(define (validate-tool-preference preference)
  (define preferred (hash-try-get preference 'prefer))
  (define selected (hash-try-get preference 'use))
  (cond
    [(and preferred (equal? preferred "same-route-hosted")) #t]
    [selected (find-named tool-implementation-registry selected) #t]
    [else (error! "invalid tool preference")]))

(define (validate-composition!)
  (if (equal? selected-prompt-builder "") (error! "no prompt builder selected"))
  (if (equal? selected-compactor "") (error! "no compactor selected"))
  (find-named prompt-builder-registry selected-prompt-builder)
  (find-named compactor-registry selected-compactor)
  (map (lambda (entry)
         (find-named tool-implementation-registry (hash-ref entry 'name)))
       tool-config-registry)
  (map (lambda (selection)
         (map validate-tool-preference (hash-ref selection 'preferences)))
       tool-selection-registry)
  #t)

(define (dispatch-command name state arguments)
  (define (find entries)
    (cond [(null? entries) (error! "unknown plugin command")]
          [(equal? name (hash-ref (hash-ref (car entries) 'spec) 'name))
           ((hash-ref (car entries) 'handler) state arguments)]
          [else (find (cdr entries))]))
  (find command-registry))
"#;

impl Policy {
    pub fn load(agent: &Path, plugins: &[PathBuf], main: &Path) -> Result<Self> {
        Self::load_with_state(
            agent,
            plugins,
            main,
            r#"{"context_char_budget":24000,"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
            None,
        )
    }

    pub fn load_with_state(
        agent: &Path,
        plugins: &[PathBuf],
        main: &Path,
        config: &str,
        state: Option<String>,
    ) -> Result<Self> {
        let mut vm = Engine::new_sandboxed();
        let mut source = String::from(PLUGIN_PRELUDE);
        for plugin in plugins {
            source.push('\n');
            source.push_str(&fs::read_to_string(plugin)?);
        }
        source.push('\n');
        source.push_str(&fs::read_to_string(main)?);
        source.push('\n');
        source.push_str(&fs::read_to_string(agent)?);
        vm.compile_and_run_raw_program(source)
            .context("load Steel policy")?;
        eval_string(
            &mut vm,
            &format!(
                "(begin (configure-runtime! {}) \"\")",
                scheme_string(config)
            ),
        )?;
        eval_string(&mut vm, "(begin (validate-composition!) \"\")")?;
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

    pub fn resolved_tools(&mut self, model: &str) -> Result<Vec<String>> {
        let encoded = eval_string(
            &mut self.vm,
            &format!(
                "(value->jsexpr-string (resolved-tool-names {}))",
                scheme_string(model)
            ),
        )?;
        serde_json::from_str(&encoded).context("decode resolved tools")
    }

    pub fn resolved_tool_routes(&mut self, model: &str) -> Result<Vec<ToolRoute>> {
        let encoded = eval_string(
            &mut self.vm,
            &format!(
                "(value->jsexpr-string (resolved-tool-routes {}))",
                scheme_string(model)
            ),
        )?;
        serde_json::from_str(&encoded).context("decode resolved tool routes")
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

pub fn composition_plugins(main: &Path) -> Result<Vec<String>> {
    let mut vm = Engine::new_sandboxed();
    let source = format!(
        r#"(require-builtin steel/json)
            (define discovered-plugins '())
            (define (load-plugin! name)
              (set! discovered-plugins (append discovered-plugins (list name))))
            (define (select-prompt-builder! _) #t)
            (define (select-compactor! _ __) #t)
            (define (configure-tool! _ __) #t)
            (define (select-tool! _ __) #t)
            {}
        "#,
        fs::read_to_string(main)?
    );
    vm.compile_and_run_raw_program(source)
        .context("load Steel composition")?;
    let encoded = eval_string(&mut vm, "(value->jsexpr-string discovered-plugins)")?;
    serde_json::from_str(&encoded).context("decode composition plugins")
}

pub fn check_plugin(entrypoint: &Path) -> Result<()> {
    let mut vm = Engine::new_sandboxed();
    vm.compile_and_run_raw_program(format!(
        "{}\n{}",
        PLUGIN_PRELUDE,
        fs::read_to_string(entrypoint)?
    ))
    .context("load Steel plugin")?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct PluginCommandOutput {
    state: serde_json::Value,
    content: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct ToolRoute {
    pub capability: String,
    pub implementation: String,
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

pub fn check(agent: &Path, plugins: &[PathBuf], main: &Path) -> Result<()> {
    let _ = Policy::load(agent, plugins, main)?;
    Ok(())
}

pub fn replay_smoke(agent: &Path, plugins: &[PathBuf], main: &Path) -> Result<()> {
    let mut policy = Policy::load_with_state(
        agent,
        plugins,
        main,
        r#"{"context_char_budget":24000,"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
        None,
    )?;
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

    fn plugins(root: &Path) -> Vec<PathBuf> {
        vec![
            root.join("policy/providers/responses.scm"),
            root.join("policy/providers/openai.scm"),
            root.join("policy/providers/openrouter.scm"),
            root.join("policy/tools/openai-web-search.scm"),
            root.join("policy/tools/openrouter-web-search.scm"),
            root.join("policy/prompts/simple.scm"),
            root.join("policy/compaction/simple.scm"),
        ]
    }

    fn policy(root: &Path) -> Policy {
        Policy::load(
            &root.join("policy/agent.scm"),
            &plugins(root),
            &root.join("main.scm"),
        )
        .unwrap()
    }

    #[test]
    fn policy_maps_events_to_provider_and_finish_effects() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
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
                    && body["tools"].as_array().unwrap().iter()
                        .any(|tool| tool["type"] == "web_search")
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
        let mut policy = policy(&root);
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
                       'tools '()))
               (register-prompt-builder! "simple" build-prompt)"#,
        )
        .unwrap();
        let custom = vec![
            root.join("policy/providers/responses.scm"),
            root.join("policy/providers/openai.scm"),
            root.join("policy/providers/openrouter.scm"),
            root.join("policy/tools/openai-web-search.scm"),
            root.join("policy/tools/openrouter-web-search.scm"),
            prompt,
            root.join("policy/compaction/simple.scm"),
        ];
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &custom,
            &root.join("main.scm"),
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
        let mut policy = policy(&root);
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
            &plugins(&root),
            &root.join("main.scm"),
            r#"{"context_char_budget":4000,"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
            None,
        )
        .unwrap();
        policy
            .on_event(&Event::UserMessage {
                content: "x".repeat(3_000),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "y".repeat(3_000)
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["model"] == "gpt-5.6-luna"
                    && body["reasoning"]["effort"] == "low"
                    && body["tools"].as_array().unwrap().is_empty()
                    && body["instructions"].as_str().unwrap().contains("Summarize")
        ));
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["activity"], "compacting");
        assert_eq!(state["compactions"], 0.0);

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "short summary"
                })],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(
            output.effects,
            vec![Effect::Finish {
                content: "y".repeat(3_000)
            }]
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["compactions"], 1.0);
        assert_eq!(state["activity"], "ready");
        assert!(serde_json::to_string(&state["messages"]).unwrap().len() <= 4_000);
        assert!(state["estimated_tokens"].as_f64().unwrap() <= 1_000.0);
    }

    #[test]
    fn compaction_truncates_oversized_tool_result() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load_with_state(
            &root.join("policy/agent.scm"),
            &plugins(&root),
            &root.join("main.scm"),
            r#"{"context_char_budget":4000,"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
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
        let output = policy
            .on_event(&Event::ToolCompleted {
                name: "shell".into(),
                result: serde_json::json!({ "stdout": "x".repeat(64 * 1024) }),
            })
            .unwrap();
        assert!(
            matches!(&output.effects[0], Effect::HttpRequest { body, .. }
            if body["instructions"].as_str().unwrap().contains("Summarize"))
        );
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "The shell returned a large listing."
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(
            matches!(&output.effects[0], Effect::HttpRequest { body, .. }
            if body["instructions"].as_str().unwrap().contains("Answer ordinary requests"))
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert!(serde_json::to_string(&state["messages"]).unwrap().len() <= 4_000);
        let messages = state["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("large listing")
        );
    }

    #[test]
    fn provider_registers_models_and_model_selection_persists() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        assert_eq!(policy.models().unwrap()[0].id, "openai/gpt-5.6-luna");
        let output = policy
            .on_event(&Event::ModelSelected {
                model: "openai/gpt-5.6-luna".into(),
                reasoning: "low".into(),
                service_tier: "default".into(),
            })
            .unwrap();
        assert!(
            matches!(&output.effects[0], Effect::Finish { content } if content.contains("gpt-5.6-luna"))
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["model"], "openai/gpt-5.6-luna");
        assert_eq!(state["reasoning"], "low");
        assert_eq!(state["service_tier"], "default");
    }

    #[test]
    fn search_routing_prefers_the_selected_provider() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let fallback = temp.path().join("fallback.scm");
        fs::write(
            &fallback,
            r#"(register-model!
                 "other"
                 (hash 'id "model" 'label "model" 'description ""
                       'function_tools #t 'hosted_tools '()
                       'reasoning '() 'default_reasoning ""
                       'service_tiers '() 'default_service_tier ""))"#,
        )
        .unwrap();
        let mut sources = plugins(&root);
        sources.push(fallback);
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &sources,
            &root.join("main.scm"),
        )
        .unwrap();

        assert_eq!(
            policy.resolved_tools("openai/gpt-5.6-sol").unwrap(),
            vec!["openai/hosted-web-search"]
        );
        assert_eq!(
            policy.resolved_tools("openrouter/x-ai/grok-4.5").unwrap(),
            vec!["openrouter/hosted-web-search"]
        );
        assert_eq!(
            policy.resolved_tools("other/model").unwrap(),
            vec!["openai/callable-web-search"]
        );
    }

    #[test]
    fn callable_search_runs_as_a_hidden_subrequest() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let fallback = temp.path().join("fallback.scm");
        fs::write(
            &fallback,
            r#"(register-provider!
                 "other" provider-effect responses-call responses-arguments
                 responses-output responses-usage
                 (lambda (events) (responses-preserved-items "openai" events))
                 responses-message-phase)
               (register-model!
                 "other"
                 (hash 'id "gpt-5.6-luna" 'label "other" 'description ""
                       'function_tools #t 'hosted_tools '()
                       'reasoning (list (hash 'id "low" 'description ""))
                       'default_reasoning "low"
                       'service_tiers
                         (list (hash 'id "default" 'description ""))
                       'default_service_tier "default"))"#,
        )
        .unwrap();
        let mut sources = plugins(&root);
        sources.push(fallback);
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &sources,
            &root.join("main.scm"),
        )
        .unwrap();
        policy
            .on_event(&Event::ModelSelected {
                model: "other/gpt-5.6-luna".into(),
                reasoning: "low".into(),
                service_tier: "default".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::UserMessage {
                content: "search".into(),
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
                        "call_id": "search-1",
                        "name": "web_search",
                        "arguments": "{\"query\":\"current Rust release\"}"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["model"] == "gpt-5.6-luna"
                    && body["tools"][0]["type"] == "web_search"
        ));
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["activity"], "searching");

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "Rust 1.97.0 (https://rust-lang.org)"
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["input"].as_array().unwrap().iter().any(|item|
                    item["type"] == "function_call_output"
                        && item["output"].as_str().unwrap().contains("Rust 1.97.0"))
        ));
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
        let custom = vec![
            root.join("policy/providers/responses.scm"),
            provider,
            root.join("policy/providers/openrouter.scm"),
            root.join("policy/tools/openai-web-search.scm"),
            root.join("policy/tools/openrouter-web-search.scm"),
            root.join("policy/prompts/simple.scm"),
            root.join("policy/compaction/simple.scm"),
        ];
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &custom,
            &root.join("main.scm"),
        )
        .unwrap();
        assert_eq!(policy.commands().unwrap()[0].name, "echo");
        assert_eq!(policy.run_command("echo", "hello").unwrap(), "hello");
    }

    #[test]
    fn malformed_tool_arguments_become_a_tool_error_input() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
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
