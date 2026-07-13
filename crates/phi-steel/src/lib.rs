use std::{fs, path::Path};

use anyhow::{Context, Result};
use phi_protocol::{Event, PolicyOutput};
use steel::{rvals::FromSteelVal, steel_vm::engine::Engine};

pub struct Policy {
    vm: Engine,
    state: String,
}

impl Policy {
    pub fn load(agent: &Path, provider: &Path, compaction: &Path) -> Result<Self> {
        Self::load_with_state(
            agent,
            provider,
            compaction,
            r#"{"context_char_budget":24000}"#,
            None,
        )
    }

    pub fn load_with_state(
        agent: &Path,
        provider: &Path,
        compaction: &Path,
        config: &str,
        state: Option<String>,
    ) -> Result<Self> {
        let mut vm = Engine::new_sandboxed();
        let source = format!(
            "{}\n{}\n{}",
            fs::read_to_string(provider)?,
            fs::read_to_string(compaction)?,
            fs::read_to_string(agent)?
        );
        vm.compile_and_run_raw_program(source)
            .context("load Steel policy")?;
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

pub fn check(agent: &Path, provider: &Path, compaction: &Path) -> Result<()> {
    let _ = Policy::load(agent, provider, compaction)?;
    Ok(())
}

pub fn replay_smoke(agent: &Path, provider: &Path, compaction: &Path) -> Result<()> {
    let mut policy = Policy::load(agent, provider, compaction)?;
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
            Effect::HttpRequest { url, .. } if url.ends_with("/responses")
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
    fn policy_continues_after_a_tool_result() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
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
    fn malformed_tool_arguments_become_a_tool_error_input() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = Policy::load(
            &root.join("policy/agent.scm"),
            &root.join("policy/providers/openai.scm"),
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
