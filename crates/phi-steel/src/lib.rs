use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use phi_protocol::{CommandSpec, Event, ModelSpec, PolicyOutput};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use steel::{SteelErr, rerrs::ErrorKind, rvals::FromSteelVal, steel_vm::engine::Engine};

pub struct Policy {
    vm: Engine,
    state: String,
}

#[derive(Debug)]
struct PolicyRejected {
    message: String,
}

impl fmt::Display for PolicyRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PolicyRejected {}

pub fn user_error_message(error: &anyhow::Error) -> Option<String> {
    for cause in error.chain() {
        if let Some(rejected) = cause.downcast_ref::<PolicyRejected>() {
            return Some(rejected.message.clone());
        }
        if cause.downcast_ref::<SteelErr>().is_some() {
            return Some("Internal policy error.".into());
        }
    }
    None
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CompositionStatus {
    pub prompt_builder: String,
    pub file_editor: String,
    pub compactor: String,
}

const PLUGIN_PRELUDE: &str = include_str!("plugin_prelude.scm");

impl Policy {
    pub fn load(config: &Path, plugins: &[PathBuf]) -> Result<Self> {
        Self::load_with_state(
            config,
            plugins,
            r#"{"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
            None,
        )
    }

    pub fn load_with_state(
        config_file: &Path,
        plugins: &[PathBuf],
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
        source.push_str(&fs::read_to_string(config_file)?);
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
        let output: PolicyOutput = eval_json(
            &mut self.vm,
            &invocation(
                "on-event",
                [scheme_string(&self.state), scheme_string(&event)],
            ),
            "decode policy output",
        )?;
        self.state.clone_from(&output.state);
        Ok(output)
    }

    pub fn commands(&mut self) -> Result<Vec<CommandSpec>> {
        eval_json(
            &mut self.vm,
            &json_invocation("registered-command-specs", []),
            "decode plugin commands",
        )
    }

    pub fn models(&mut self) -> Result<Vec<ModelSpec>> {
        eval_json(
            &mut self.vm,
            &json_invocation("registered-models", []),
            "decode provider models",
        )
    }

    pub fn resolved_tools(&mut self, model: &str) -> Result<Vec<String>> {
        eval_json(
            &mut self.vm,
            &json_invocation("resolved-tool-names", [scheme_string(model)]),
            "decode resolved tools",
        )
    }

    pub fn resolved_tool_routes(&mut self, model: &str) -> Result<Vec<ToolRoute>> {
        eval_json(
            &mut self.vm,
            &json_invocation("resolved-tool-routes", [scheme_string(model)]),
            "decode resolved tool routes",
        )
    }

    pub fn file_editor_tool_name(&mut self) -> Result<String> {
        eval_string(&mut self.vm, "(selected-file-editor-tool-name)")
    }

    pub fn composition_status(&mut self) -> Result<CompositionStatus> {
        eval_json(
            &mut self.vm,
            &json_invocation("composition-status", []),
            "decode composition status",
        )
    }

    pub fn prepare_file_edit(
        &mut self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        eval_json(
            &mut self.vm,
            &json_invocation(
                "prepare-file-edit",
                [scheme_string(name), json_argument(arguments)?],
            ),
            "decode file edit preparation",
        )
    }

    pub fn propose_file_edit(
        &mut self,
        name: &str,
        plan: &serde_json::Value,
        snapshots: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        eval_json(
            &mut self.vm,
            &json_invocation(
                "propose-file-edit",
                [
                    scheme_string(name),
                    json_argument(plan)?,
                    json_argument(snapshots)?,
                ],
            ),
            "decode proposed file changes",
        )
    }

    pub fn complete_callable_tool(
        &mut self,
        implementation: &str,
        events: &[serde_json::Value],
    ) -> Result<serde_json::Value> {
        eval_json(
            &mut self.vm,
            &json_invocation(
                "complete-callable-tool",
                [
                    invocation(
                        "find-named",
                        [
                            "tool-implementation-registry".to_owned(),
                            scheme_string(implementation),
                        ],
                    ),
                    json_argument(events)?,
                ],
            ),
            "decode callable tool result",
        )
    }

    pub fn run_command(&mut self, name: &str, arguments: &str) -> Result<String> {
        let output: PluginCommandOutput = eval_json(
            &mut self.vm,
            &json_invocation(
                "dispatch-command",
                [
                    scheme_string(name),
                    encoded_json_argument(&self.state),
                    scheme_string(arguments),
                ],
            ),
            "decode plugin command output",
        )?;
        self.state = serde_json::to_string(&output.state)?;
        Ok(output.content)
    }
}

pub fn composition_plugins(config: &Path) -> Result<Vec<String>> {
    let mut vm = Engine::new_sandboxed();
    let source = format!(
        r#"{}
            (define discovered-plugins '())
            (set! load-plugin!
              (lambda (name)
                (set! discovered-plugins (append discovered-plugins (list name)))))
            {}
        "#,
        PLUGIN_PRELUDE,
        fs::read_to_string(config)?
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
        .map_err(policy_error)?;
    let value = values.last().context("Steel policy returned no value")?;
    String::from_steelval(value).map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn eval_json<T: DeserializeOwned>(
    vm: &mut Engine,
    expression: &str,
    error_label: &'static str,
) -> Result<T> {
    let encoded = eval_string(vm, expression)?;
    serde_json::from_str(&encoded).context(error_label)
}

fn invocation(function: &str, arguments: impl IntoIterator<Item = String>) -> String {
    let arguments = arguments.into_iter().collect::<Vec<_>>().join(" ");
    if arguments.is_empty() {
        format!("({function})")
    } else {
        format!("({function} {arguments})")
    }
}

fn json_invocation(function: &str, arguments: impl IntoIterator<Item = String>) -> String {
    invocation("value->jsexpr-string", [invocation(function, arguments)])
}

fn json_argument<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(encoded_json_argument(&serde_json::to_string(value)?))
}

fn encoded_json_argument(value: &str) -> String {
    invocation("string->jsexpr", [scheme_string(value)])
}

fn policy_error(error: SteelErr) -> anyhow::Error {
    if error.kind() == ErrorKind::Generic {
        let rendered = error.to_string();
        let prefix = format!("Error: {}:", error.kind());
        return anyhow::Error::new(PolicyRejected {
            message: rendered
                .strip_prefix(&prefix)
                .unwrap_or(&rendered)
                .trim()
                .to_owned(),
        });
    }
    anyhow::Error::new(error).context("run policy")
}

fn scheme_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

pub fn check(config: &Path, plugins: &[PathBuf]) -> Result<()> {
    let _ = Policy::load(config, plugins)?;
    Ok(())
}

pub fn replay_smoke(config: &Path, plugins: &[PathBuf]) -> Result<()> {
    let mut policy = Policy::load_with_state(
        config,
        plugins,
        r#"{"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
        None,
    )?;
    let output = policy.on_event(&Event::UserMessage {
        content: "replay fixture".into(),
    })?;
    if output.effects.is_empty() {
        anyhow::bail!("configuration replay emitted no effects");
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
            root.join("policy/tools/skills.scm"),
            root.join("policy/tools/codex-patch.scm"),
            root.join("policy/prompts/simple.scm"),
            root.join("policy/compaction/structured.scm"),
        ]
    }

    fn policy(root: &Path) -> Policy {
        Policy::load(&root.join("config.scm"), &plugins(root)).unwrap()
    }

    fn compact_policy(root: &Path, limit: u64) -> Policy {
        compact_policy_with_strict(root, limit, true)
    }

    fn compact_policy_with_strict(root: &Path, limit: u64, strict: bool) -> Policy {
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("openai.scm");
        let mut source = fs::read_to_string(root.join("policy/providers/openai.scm"))
            .unwrap()
            .replace(
                "'compaction_token_limit 244800",
                &format!("'compaction_token_limit {limit}"),
            );
        if !strict {
            source = source.replace(
                "'strict_json_schema_capable #t",
                "'strict_json_schema_capable #f",
            );
        }
        fs::write(&provider, source).unwrap();
        let mut sources = plugins(root);
        sources[1] = provider;
        Policy::load(&root.join("config.scm"), &sources).unwrap()
    }

    #[test]
    fn eval_json_decodes_typed_output() {
        let mut vm = Engine::new_sandboxed();
        let encoded =
            r#"{"prompt_builder":"simple","file_editor":"patch","compactor":"structured"}"#;

        let status: CompositionStatus =
            eval_json(&mut vm, &scheme_string(encoded), "decode test output").unwrap();

        assert_eq!(
            status,
            CompositionStatus {
                prompt_builder: "simple".into(),
                file_editor: "patch".into(),
                compactor: "structured".into(),
            }
        );
    }

    #[test]
    fn malformed_policy_output_keeps_decode_error_label() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        let state = policy.state().to_owned();
        eval_string(
            &mut policy.vm,
            r#"(begin (set! on-event (lambda (_state _event) "not json")) "")"#,
        )
        .unwrap();

        let error = policy
            .on_event(&Event::UserMessage {
                content: "hello".into(),
            })
            .unwrap_err();

        assert_eq!(error.to_string(), "decode policy output");
        assert_eq!(policy.state(), state);
    }

    #[test]
    fn composition_discovery_uses_every_configuration_primitive() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.scm");
        fs::write(
            &config,
            r#"
                (configure-runtime! "{\"tools\":[],\"session_id\":\"discovery\"}")
                (set-agent-instructions! "instructions")
                (register-command! (hash 'name "command") (lambda (_state _arguments) (hash)))
                (register-tool! (lambda () (hash 'name "plugin-tool")))
                (register-provider!
                  "provider"
                  (lambda (_prompt _model _reasoning _tier) (hash))
                  (lambda (_events) '())
                  (lambda (_call) (hash))
                  (lambda (_events) "")
                  (lambda (_events) #f)
                  (lambda (_events) '())
                  (lambda (_events) #f))
                (register-model! "provider" (hash 'id "model" 'function_tools #t))
                (register-model! "provider" (hash 'id "removed"))
                (unregister-model! "provider/removed")
                (register-hosted-tool!
                  "hosted" "search" "provider" (lambda (_config) (hash)))
                (register-callable-tool!
                  "callable" "command" #t (hash 'name "callable")
                  (lambda (_arguments _config) (hash))
                  (lambda (_events _config) (hash)))
                (configure-tool! "hosted" (hash 'enabled #t))
                (select-tool! "search" (list (hash 'use "hosted")))
                (register-prompt-builder!
                  "prompt" (lambda (_messages _instructions _tools) (hash)))
                (register-compactor!
                  "compactor"
                  (lambda (_messages _usage _max-tokens _config) #f)
                  (lambda (_messages _max-tokens _config) (hash))
                  (lambda (_messages _usage _max-tokens _events _repair-count _config) '()))
                (register-file-editor!
                  "editor" (hash 'name "edit")
                  (lambda (_arguments) (hash))
                  (lambda (_plan _snapshots) '()))
                (select-prompt-builder! "prompt")
                (select-compactor! "compactor" (hash))
                (select-file-editor! "editor")
                (load-plugin! "first")
                (load-plugin! "second")
                (validate-composition!)
            "#,
        )
        .unwrap();

        assert_eq!(
            composition_plugins(&config).unwrap(),
            vec!["first".to_owned(), "second".to_owned()]
        );
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
            root.join("policy/tools/codex-patch.scm"),
            prompt,
            root.join("policy/compaction/structured.scm"),
        ];
        let mut policy = Policy::load(&root.join("config.scm"), &custom).unwrap();
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
                        "arguments": "{\"path\":\"config.scm\"}"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::RunTools { calls }
            if calls.len() == 1 && calls[0].name == "read_file"));
        let output = policy
            .on_event(&Event::ToolsCompleted {
                results: vec![phi_protocol::ToolResult {
                    call_id: "call-1".into(),
                    name: "read_file".into(),
                    result: serde_json::json!({ "content": "policy" }),
                }],
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::HttpRequest { .. }));
    }

    #[test]
    fn policy_returns_all_tool_calls_in_one_batch() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        policy
            .on_event(&Event::UserMessage {
                content: "inspect both".into(),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: ["a.rs", "b.rs"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, path)| {
                        serde_json::json!({
                            "type": "response.output_item.done",
                            "item": {
                                "type": "function_call",
                                "call_id": format!("call-{index}"),
                                "name": "read_file",
                                "arguments": serde_json::json!({ "path": path }).to_string()
                            }
                        })
                    })
                    .collect(),
                error: String::new(),
            })
            .unwrap();
        let Effect::RunTools { calls } = &output.effects[0] else {
            panic!("expected tool batch");
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id, "call-0");
        assert_eq!(calls[1].arguments["path"], "b.rs");

        let output = policy
            .on_event(&Event::ToolsCompleted {
                results: calls
                    .iter()
                    .map(|call| phi_protocol::ToolResult {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        result: serde_json::json!({ "content": call.arguments["path"] }),
                    })
                    .collect(),
            })
            .unwrap();
        assert!(
            matches!(&output.effects[0], Effect::HttpRequest { body, .. }
            if body["input"].as_array().unwrap().iter().filter(|item|
                item["type"] == "function_call_output").count() == 2)
        );
    }

    #[test]
    fn codex_patch_plugin_prepares_and_proposes_file_changes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        assert_eq!(policy.file_editor_tool_name().unwrap(), "patch");

        let preparation = policy
            .prepare_file_edit(
                "patch",
                &serde_json::json!({
                    "patch": concat!(
                        "*** Begin Patch\n",
                        "*** Update File: src/main.rs\n",
                        "@@ mod app {\n",
                        "@@ fn main() {\n",
                        "-    old();\n",
                        "+    new();\n",
                        "*** Add File: src/new.rs\n",
                        "+pub fn added() {}\n",
                        "*** Delete File: src/old.rs\n",
                        "*** End Patch\n"
                    )
                }),
            )
            .unwrap();
        assert_eq!(preparation["targets"].as_array().unwrap().len(), 3);

        let changes = policy
            .propose_file_edit(
                "patch",
                &preparation["plan"],
                &serde_json::json!([
                    {
                        "path": "src/main.rs",
                        "exists": true,
                        "content": "mod app {\nfn main() {\n    old();\n}\n}\n",
                        "revision": "one"
                    },
                    {
                        "path": "src/new.rs",
                        "exists": false,
                        "content": "",
                        "revision": ""
                    },
                    {
                        "path": "src/old.rs",
                        "exists": true,
                        "content": "old\n",
                        "revision": "two"
                    }
                ]),
            )
            .unwrap();
        assert_eq!(changes[0]["operation"], "replace");
        assert_eq!(
            changes[0]["content"],
            "mod app {\nfn main() {\n    new();\n}\n}\n"
        );
        assert_eq!(changes[1]["operation"], "create");
        assert_eq!(changes[1]["content"], "pub fn added() {}\n");
        assert_eq!(changes[2]["operation"], "delete");
    }

    #[test]
    fn codex_patch_accepts_context_only_locator_hunks() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        let preparation = policy
            .prepare_file_edit(
                "patch",
                &serde_json::json!({
                    "patch": concat!(
                        "*** Begin Patch\n",
                        "*** Update File: crates/phi-runtime/src/workflow.rs\n",
                        "@@\n",
                        "     async fn launches_and_inspects_a_plugin_workflow() {\n",
                        "@@\n",
                        "         tasks.shutdown().await;\n",
                        "     }\n",
                        "+\n",
                        "+    #[tokio::test]\n",
                        "+    async fn added_workflow_test() {}\n",
                        "*** End Patch\n"
                    )
                }),
            )
            .unwrap();

        let changes = policy
            .propose_file_edit(
                "patch",
                &preparation["plan"],
                &serde_json::json!([{
                    "path": "crates/phi-runtime/src/workflow.rs",
                    "exists": true,
                    "content": concat!(
                        "mod tests {\n",
                        "    async fn launches_and_inspects_a_plugin_workflow() {\n",
                        "        tasks.shutdown().await;\n",
                        "    }\n",
                        "}\n"
                    ),
                    "revision": "one"
                }]),
            )
            .unwrap();

        assert_eq!(
            changes[0]["content"],
            concat!(
                "mod tests {\n",
                "    async fn launches_and_inspects_a_plugin_workflow() {\n",
                "        tasks.shutdown().await;\n",
                "    }\n",
                "\n",
                "    #[tokio::test]\n",
                "    async fn added_workflow_test() {}\n",
                "}\n"
            )
        );
    }

    #[test]
    fn policy_rejections_expose_only_the_domain_message() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        let error = policy
            .prepare_file_edit(
                "patch",
                &serde_json::json!({
                    "patch": concat!(
                        "*** Begin Patch\n",
                        "*** Update File: src/main.rs\n",
                        "@@ fn main() {\n",
                        " unchanged();\n",
                        "*** End Patch\n"
                    )
                }),
            )
            .unwrap_err();

        assert_eq!(
            user_error_message(&error).as_deref(),
            Some("src/main.rs: patch makes no change")
        );
        assert!(!error.to_string().contains("Steel"));
        assert!(!error.to_string().contains("policy"));
    }

    #[test]
    fn codex_patch_rejects_semantically_unchanged_updates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        let preparation = policy
            .prepare_file_edit(
                "patch",
                &serde_json::json!({
                    "patch": concat!(
                        "*** Begin Patch\n",
                        "*** Update File: src/main.rs\n",
                        "@@ fn main() {\n",
                        "-    unchanged();\n",
                        "+    unchanged();\n",
                        "*** End Patch\n"
                    )
                }),
            )
            .unwrap();
        let error = policy
            .propose_file_edit(
                "patch",
                &preparation["plan"],
                &serde_json::json!([{
                    "path": "src/main.rs",
                    "exists": true,
                    "content": "fn main() {\n    unchanged();\n}\n",
                    "revision": "one"
                }]),
            )
            .unwrap_err();

        assert_eq!(
            user_error_message(&error).as_deref(),
            Some("src/main.rs: patch makes no change")
        );
    }

    #[test]
    fn codex_patch_errors_identify_the_file_and_hunk() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        let preparation = policy
            .prepare_file_edit(
                "patch",
                &serde_json::json!({
                    "patch": concat!(
                        "*** Begin Patch\n",
                        "*** Update File: src/main.rs\n",
                        "@@\n",
                        "-missing();\n",
                        "+replacement();\n",
                        "*** End Patch\n"
                    )
                }),
            )
            .unwrap();
        let error = policy
            .propose_file_edit(
                "patch",
                &preparation["plan"],
                &serde_json::json!([{
                    "path": "src/main.rs",
                    "exists": true,
                    "content": "present();\n",
                    "revision": "one"
                }]),
            )
            .unwrap_err();

        assert_eq!(
            user_error_message(&error).as_deref(),
            Some("src/main.rs: hunk 1 context not found")
        );
    }

    #[test]
    fn unexpected_steel_errors_are_hidden_from_users() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = policy(&root);
        let error = eval_string(&mut policy.vm, "missing-policy-function").unwrap_err();

        assert_eq!(
            user_error_message(&error).as_deref(),
            Some("Internal policy error.")
        );
        assert!(format!("{error:#}").contains("FreeIdentifier"));
    }

    #[test]
    fn manual_compaction_runs_below_the_automatic_threshold() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 30_000);
        policy
            .on_event(&Event::UserMessage {
                content: "short conversation".into(),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "short answer"
                })],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(
            output.effects,
            vec![Effect::Finish {
                content: "short answer".into()
            }]
        );

        let output = policy.on_event(&Event::CompactRequested).unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["instructions"].as_str().unwrap().contains("Summarize")
                    && body["text"]["format"]["type"] == "json_schema"
                    && body["text"]["format"]["strict"] == true
                    && body["text"]["format"]["schema"]["required"]
                        .as_array().unwrap().len() == 5
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
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["manually compacted"],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(
            output.effects,
            vec![Effect::Finish {
                content: "Compaction complete.".into()
            }]
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["activity"], "ready");
        assert_eq!(state["compactions"], 1.0);
    }

    #[test]
    fn compaction_repairs_unstructured_model_output() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 30_000);
        policy
            .on_event(&Event::UserMessage {
                content: "investigate the design".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "findings"
                })],
                error: String::new(),
            })
            .unwrap();
        policy.on_event(&Event::CompactRequested).unwrap();

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "# Investigation report\n\nThe model ignored the JSON request."
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["input"].as_array().unwrap().iter().any(|item|
                    item["role"] == "user"
                        && item["content"][0]["text"].as_str().unwrap()
                            .contains("previous response was not valid JSON"))
        ));
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["compactions"], 0.0);
        assert_eq!(state["compaction_attempt"], 1.0);

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["repaired"],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();
        assert_eq!(
            output.effects,
            vec![Effect::Finish {
                content: "Compaction complete.".into()
            }]
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["compactions"], 1.0);
        assert_eq!(state["compaction_attempt"], 0.0);
    }

    #[test]
    fn compaction_repairs_empty_model_output() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 30_000);
        policy
            .on_event(&Event::UserMessage {
                content: "empty output test".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "answer"
                })],
                error: String::new(),
            })
            .unwrap();
        policy.on_event(&Event::CompactRequested).unwrap();

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: Vec::new(),
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::HttpRequest { .. }));
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["compaction_attempt"], 1.0);
        assert_eq!(state["compactions"], 0.0);
    }

    #[test]
    fn compaction_uses_prompt_fallback_for_non_strict_models() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy_with_strict(&root, 30_000, false);
        policy
            .on_event(&Event::UserMessage {
                content: "fallback test".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "answer"
                })],
                error: String::new(),
            })
            .unwrap();
        let output = policy.on_event(&Event::CompactRequested).unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body.get("text").is_none()
                    && body["instructions"].as_str().unwrap().contains("JSON only")
        ));
    }

    #[test]
    fn compaction_stops_after_four_repair_attempts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 30_000);
        policy
            .on_event(&Event::UserMessage {
                content: "retry test".into(),
            })
            .unwrap();
        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "answer"
                })],
                error: String::new(),
            })
            .unwrap();
        policy.on_event(&Event::CompactRequested).unwrap();

        for attempt in 1..=4 {
            let output = policy
                .on_event(&Event::HttpCompleted {
                    success: true,
                    status: 200,
                    events: vec![serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "not json"
                    })],
                    error: String::new(),
                })
                .unwrap();
            assert!(matches!(&output.effects[0], Effect::HttpRequest { .. }));
            let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
            assert_eq!(state["compaction_attempt"], attempt as f64);
        }

        let error = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "still not json"
                })],
                error: String::new(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("after 4 repair attempts"));
    }

    #[test]
    fn compaction_bounds_large_context() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 1_000);
        policy
            .on_event(&Event::UserMessage {
                content: "x".repeat(3_000),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "reasoning",
                            "id": "reasoning-before-compaction",
                            "encrypted_content": "opaque"
                        }
                    }),
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "y".repeat(3_000)
                    }),
                ],
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
                    && body["instructions"].as_str().unwrap().contains("current_state")
                    && body["input"].as_array().unwrap().iter()
                        .any(|item| item["type"] == "reasoning"
                            && item["encrypted_content"] == "opaque")
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
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["context compacted"],"pending":[],"next_steps":[]}"#
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
        assert!(
            state["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("Conversation state (JSON):\n{")
        );
        assert!(serde_json::to_string(&state["messages"]).unwrap().len() <= 4_000);
        assert!(state["estimated_tokens"].as_f64().unwrap() <= 1_000.0);
    }

    #[test]
    fn structured_compaction_retains_the_last_sixteen_messages() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 30_000);
        policy
            .on_event(&Event::UserMessage {
                content: "old".repeat(50_000),
            })
            .unwrap();
        let mut events = (0..20)
            .map(|index| {
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "reasoning",
                        "id": format!("reasoning-{index}"),
                        "encrypted_content": format!("opaque-{index}")
                    }
                })
            })
            .collect::<Vec<_>>();
        events.push(serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "latest answer"
        }));
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events,
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["instructions"].as_str().unwrap().contains("current_state")
        ));

        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["compacted"],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();

        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        let messages = state["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 17);
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("Conversation state (JSON):\n{")
        );
        assert_eq!(messages[1]["item"]["id"], "reasoning-5");
        assert_eq!(messages[16]["content"], "latest answer");
    }

    #[test]
    fn structured_compaction_drops_tool_results_whose_calls_fall_out_of_the_tail() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 5_000);
        policy
            .on_event(&Event::UserMessage {
                content: "x".repeat(25_000),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: (0..9)
                    .map(|index| {
                        serde_json::json!({
                            "type": "response.output_item.done",
                            "item": {
                                "type": "function_call",
                                "call_id": format!("call-{index}"),
                                "name": "read_file",
                                "arguments": "{}"
                            }
                        })
                    })
                    .collect(),
                error: String::new(),
            })
            .unwrap();
        let Effect::RunTools { calls } = &output.effects[0] else {
            panic!("expected tool batch");
        };
        assert_eq!(calls.len(), 9);

        let output = policy
            .on_event(&Event::ToolsCompleted {
                results: calls
                    .iter()
                    .map(|call| phi_protocol::ToolResult {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        result: serde_json::json!({ "content": "done" }),
                    })
                    .collect(),
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::HttpRequest { .. }));

        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["compacted"],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();
        let Effect::HttpRequest { body, .. } = &output.effects[0] else {
            panic!("expected continuation request");
        };
        let input = body["input"].as_array().unwrap();
        let call_ids = input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["call_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        let output_ids = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["call_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(call_ids, output_ids);
        assert_eq!(call_ids.len(), 7);
        assert!(!call_ids.contains(&"call-0"));
        assert!(!call_ids.contains(&"call-1"));
    }

    #[test]
    fn manual_compaction_repairs_an_existing_orphaned_tool_result() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let state = serde_json::json!({
            "messages": [
                { "kind": "message", "role": "user", "content": "summary" },
                { "kind": "tool_result", "call_id": "orphan", "content": "lost call" },
                { "kind": "tool_call", "call_id": "paired", "name": "read_file", "arguments": "{}" },
                { "kind": "tool_result", "call_id": "paired", "content": "result" }
            ],
            "estimated_tokens": 10,
            "compactions": 1,
            "last_usage": {},
            "model": "openai/gpt-5.6-luna",
            "reasoning": "low",
            "service_tier": "default",
            "activity": "ready",
            "pending_finish": "",
            "context_window": 272000
        });
        let mut policy = Policy::load_with_state(
            &root.join("config.scm"),
            &plugins(&root),
            r#"{"model":"openai/gpt-5.6-luna","reasoning":"low","service_tier":"default"}"#,
            Some(state.to_string()),
        )
        .unwrap();

        let output = policy.on_event(&Event::CompactRequested).unwrap();
        let Effect::HttpRequest { body, .. } = &output.effects[0] else {
            panic!("expected compaction request");
        };
        let input = body["input"].as_array().unwrap();
        assert!(!input.iter().any(|item| item["call_id"] == "orphan"));
        assert!(
            input
                .iter()
                .any(|item| { item["type"] == "function_call" && item["call_id"] == "paired" })
        );
        assert!(
            input.iter().any(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "paired"
            })
        );
    }

    #[test]
    fn structured_compaction_stops_before_the_twenty_four_thousand_token_tail_cap() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 30_000);
        policy
            .on_event(&Event::UserMessage {
                content: "oversized recent message ".repeat(10_000),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "latest answer"
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(&output.effects[0], Effect::HttpRequest { .. }));

        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["compacted"],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();

        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        let messages = state["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("Conversation state (JSON):\n{")
        );
        assert_eq!(messages[1]["content"], "latest answer");
    }

    #[test]
    fn provider_total_tokens_trigger_and_anchor_compaction() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 6_000);
        policy
            .on_event(&Event::UserMessage {
                content: "x".repeat(3_000),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "delta": "answer"
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "usage": {
                                "input_tokens": 6480,
                                "output_tokens": 20,
                                "total_tokens": 6500
                            }
                        }
                    }),
                ],
                error: String::new(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["instructions"].as_str().unwrap().contains("Summarize")
        ));

        policy
            .on_event(&Event::HttpCompleted {
                success: true,
                status: 200,
                events: vec![serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": r#"{"objective":"continue","requirements":[],"current_state":["context compacted"],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        let context = state["estimated_tokens"].as_f64().unwrap();
        assert!(context < 6_000.0);
        assert!(context > 5_000.0);
    }

    #[test]
    fn compaction_truncates_oversized_tool_result() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut policy = compact_policy(&root, 1_000);
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
                        "name": "exec_command",
                        "arguments": "{}"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        let output = policy
            .on_event(&Event::ToolsCompleted {
                results: vec![phi_protocol::ToolResult {
                    call_id: "call-1".into(),
                    name: "exec_command".into(),
                    result: serde_json::json!({ "stdout": "x".repeat(64 * 1024) }),
                }],
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
                    "delta": r#"{"objective":"list files","requirements":[],"current_state":["The shell returned a large listing."],"pending":[],"next_steps":[]}"#
                })],
                error: String::new(),
            })
            .unwrap();
        assert!(
            matches!(&output.effects[0], Effect::HttpRequest { body, .. }
            if body["instructions"].as_str().unwrap().contains("running inside a Phi harness"))
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
    fn config_can_replace_remove_and_add_provider_models() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.scm");
        let mut source = fs::read_to_string(root.join("config.scm")).unwrap();
        source.push_str(
            r#"
(register-model!
  "openai"
  (hash 'id "gpt-5.6-luna" 'label "Configured Luna" 'description ""
        'context_window 1000 'compaction_token_limit 900
        'function_tools #t 'hosted_tools '()
        'reasoning '() 'default_reasoning ""
        'service_tiers '() 'default_service_tier ""))
(unregister-model! "openrouter/anthropic/claude-sonnet-4.6")
(register-model!
  "openrouter"
  (hash 'id "minimax/minimax-m3" 'label "MiniMax M3" 'description ""
        'context_window 1000 'compaction_token_limit 900
        'function_tools #t 'hosted_tools '()
        'reasoning '() 'default_reasoning ""
        'service_tiers '() 'default_service_tier ""))
"#,
        );
        fs::write(&config, source).unwrap();

        let mut policy = Policy::load(&config, &plugins(&root)).unwrap();
        let models = policy.models().unwrap();
        assert_eq!(
            models
                .iter()
                .filter(|model| model.id == "openai/gpt-5.6-luna")
                .count(),
            1
        );
        assert_eq!(
            models
                .iter()
                .find(|model| model.id == "openai/gpt-5.6-luna")
                .unwrap()
                .label,
            "Configured Luna"
        );
        assert!(
            models
                .iter()
                .all(|model| model.id != "openrouter/anthropic/claude-sonnet-4.6")
        );
        assert!(
            models
                .iter()
                .any(|model| model.id == "openrouter/minimax/minimax-m3")
        );
    }

    #[test]
    fn skills_plugin_exposes_only_discovered_skills() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = serde_json::json!({
            "model": "openai/gpt-5.6-luna",
            "reasoning": "low",
            "service_tier": "default",
            "skills": [{ "name": "review", "description": "Review code." }]
        });
        let mut policy = Policy::load_with_state(
            &root.join("config.scm"),
            &plugins(&root),
            &config.to_string(),
            None,
        )
        .unwrap();
        let output = policy
            .on_event(&Event::UserMessage {
                content: "$review this".into(),
            })
            .unwrap();
        assert!(matches!(
            &output.effects[0],
            Effect::HttpRequest { body, .. }
                if body["tools"].as_array().unwrap().iter().any(|tool|
                    tool["name"] == "load_skill"
                        && tool["description"].as_str().unwrap().contains("review"))
        ));
        assert_eq!(
            policy.run_command("skills", "").unwrap(),
            "- review: Review code."
        );
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
                       'context_window 1000 'compaction_token_limit 900
                       'function_tools #t 'hosted_tools '()
                       'reasoning '() 'default_reasoning ""
                       'service_tiers '() 'default_service_tier ""))"#,
        )
        .unwrap();
        let mut sources = plugins(&root);
        sources.push(fallback);
        let mut policy = Policy::load(&root.join("config.scm"), &sources).unwrap();

        assert_eq!(
            policy.resolved_tools("openai/gpt-5.6-sol").unwrap(),
            vec!["openai/hosted-web-search"]
        );
        assert_eq!(
            policy
                .resolved_tools("openrouter/anthropic/claude-sonnet-4.6")
                .unwrap(),
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
                 "other" provider-effect responses-calls responses-arguments
                 responses-output responses-usage
                 (lambda (events) (responses-preserved-items "openai" events))
                 responses-message-phase)
               (register-model!
                 "other"
                 (hash 'id "gpt-5.6-luna" 'label "other" 'description ""
                       'context_window 1000 'compaction_token_limit 900
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
        let mut policy = Policy::load(&root.join("config.scm"), &sources).unwrap();
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
            Effect::RunTools { calls }
                if matches!(&calls[0].execution,
                    phi_protocol::ToolExecution::Http { body, .. }
                if body["model"] == "gpt-5.6-luna"
                    && body["tools"][0]["type"] == "web_search")
        ));
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["activity"], "working");

        let result = policy
            .complete_callable_tool(
                "openai/callable-web-search",
                &[serde_json::json!({
                    "type": "response.output_text.delta",
                    "delta": "Rust 1.97.0 (https://rust-lang.org)"
                })],
            )
            .unwrap();
        let output = policy
            .on_event(&Event::ToolsCompleted {
                results: vec![phi_protocol::ToolResult {
                    call_id: "search-1".into(),
                    name: "web_search".into(),
                    result,
                }],
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
            root.join("policy/tools/codex-patch.scm"),
            root.join("policy/prompts/simple.scm"),
            root.join("policy/compaction/structured.scm"),
        ];
        let mut policy = Policy::load(&root.join("config.scm"), &custom).unwrap();
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
            Effect::RunTools { calls }
                if calls[0].arguments.get("malformed_arguments").is_some()
        ));
    }

    #[test]
    fn direct_tool_arguments_preserve_json_integers() {
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
                        "arguments": "{\"path\":\"config.scm\",\"start_line\":1,\"line_count\":200}"
                    }
                })],
                error: String::new(),
            })
            .unwrap();
        let Effect::RunTools { calls } = &output.effects[0] else {
            panic!("expected direct tool call");
        };
        assert_eq!(calls[0].arguments["start_line"].as_u64(), Some(1));
        assert_eq!(calls[0].arguments["line_count"].as_u64(), Some(200));
    }
}
