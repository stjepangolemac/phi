use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use phi_protocol::{CommandSpec, Event, ModelSpec, PolicyOutput};
use serde::{Deserialize, Serialize};
use steel::{rvals::FromSteelVal, steel_vm::engine::Engine};

pub struct Policy {
    vm: Engine,
    state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CompositionStatus {
    pub prompt_builder: String,
    pub file_editor: String,
    pub compactor: String,
}

const PLUGIN_PRELUDE: &str = r#"
(require-builtin steel/json)
(require-builtin steel/hash)

(define command-registry '())
(define model-registry '())
(define tool-registry '())
(define plugin-tool-registry '())
(define tool-implementation-registry '())
(define tool-selection-registry '())
(define tool-config-registry '())
(define provider-registry '())
(define prompt-builder-registry '())
(define compactor-registry '())
(define file-editor-registry '())
(define selected-prompt-builder "")
(define selected-compactor "")
(define selected-compactor-config (hash))
(define selected-file-editor "")
(define agent-instructions "")
(define session-id "")
(define runtime-config (hash))

(define (configure-runtime! encoded-config)
  (define config (string->jsexpr encoded-config))
  (set! runtime-config config)
  (set! tool-registry (or (hash-try-get config 'tools) '()))
  (set! session-id (or (hash-try-get config 'session_id) "")))

(define (set-agent-instructions! value)
  (set! agent-instructions value))

(define (register-command! spec handler)
  (set! command-registry
        (append command-registry (list (hash 'spec spec 'handler handler)))))

(define (register-tool! builder)
  (set! plugin-tool-registry (append plugin-tool-registry (list builder))))

(define (register-provider! name effect call arguments output usage preserved phase)
  (set! provider-registry
        (append provider-registry
                (list (hash 'name name 'effect effect 'call call
                            'arguments arguments 'output output 'usage usage
                            'preserved preserved 'phase phase)))))

(define (remove-model-by-id models id)
  (cond [(null? models) '()]
        [(equal? id (hash-ref (car models) 'id))
         (remove-model-by-id (cdr models) id)]
        [else (cons (car models) (remove-model-by-id (cdr models) id))]))

(define (register-model! provider spec)
  (define model (hash-ref spec 'id))
  (define id (string-append provider "/" model))
  (set! model-registry
        (append (remove-model-by-id model-registry id)
                (list (hash-insert
                        (hash-insert
                          (hash-insert spec 'provider provider)
                          'model model)
                        'id id)))))

(define (unregister-model! id)
  (set! model-registry (remove-model-by-id model-registry id)))

(define (register-hosted-tool! name capability provider build)
  (set! tool-implementation-registry
        (append tool-implementation-registry
                (list (hash 'name name 'capability capability 'mode "hosted"
                            'provider provider 'build build)))))

(define (register-callable-tool! name capability parallel spec start complete)
  (set! tool-implementation-registry
        (append tool-implementation-registry
                (list (hash 'name name 'capability capability 'mode "callable"
                            'parallel parallel 'spec spec
                            'start start 'complete complete)))))

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

(define (register-file-editor! name spec prepare propose)
  (set! file-editor-registry
        (append file-editor-registry
                (list (hash 'name name 'spec spec 'prepare prepare
                            'propose propose)))))

(define (select-prompt-builder! name) (set! selected-prompt-builder name))
(define (select-compactor! name config)
  (set! selected-compactor name)
  (set! selected-compactor-config config))
(define (select-file-editor! name) (set! selected-file-editor name))

(define (composition-status)
  (hash 'prompt_builder selected-prompt-builder
        'file_editor selected-file-editor
        'compactor selected-compactor))
(define (load-plugin! _) #t)

(define (registered-command-specs)
  (map (lambda (entry) (hash-ref entry 'spec)) command-registry))

(define (registered-models) model-registry)
(define (runtime-config-value name fallback)
  (or (hash-try-get runtime-config name) fallback))

(define (built-plugin-tools builders)
  (cond [(null? builders) '()]
        [else
         (define tool ((car builders)))
         (if tool
             (cons tool (built-plugin-tools (cdr builders)))
             (built-plugin-tools (cdr builders)))]))

(define (registered-tools)
  (append tool-registry (built-plugin-tools plugin-tool-registry)))
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
  (append (registered-tools)
          (if (equal? selected-file-editor "")
              '()
              (list (hash-ref
                      (find-named file-editor-registry selected-file-editor)
                      'spec)))
          (map resolved-tool-spec (resolved-tool-implementations model))))

(define (selected-file-editor-entry)
  (find-named file-editor-registry selected-file-editor))

(define (selected-file-editor-tool-name)
  (if (equal? selected-file-editor "")
      ""
      (hash-ref (hash-ref (selected-file-editor-entry) 'spec) 'name)))

(define (prepare-file-edit name arguments)
  (if (not (equal? name (selected-file-editor-tool-name)))
      (error! (string-append "unknown file editor tool: " name)))
  ((hash-ref (selected-file-editor-entry) 'prepare) arguments))

(define (propose-file-edit name plan snapshots)
  (if (not (equal? name (selected-file-editor-tool-name)))
      (error! (string-append "unknown file editor tool: " name)))
  ((hash-ref (selected-file-editor-entry) 'propose) plan snapshots))

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
(define (provider-calls-for model events)
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

(define (estimated-message-tokens messages)
  (quotient (string-length (value->jsexpr-string messages)) 4))

(define (estimated-fixed-tokens messages usage)
  (define total (hash-try-get usage 'total_tokens))
  (define baseline (hash-try-get usage '_message_tokens))
  (if (and total baseline (> total baseline)) (- total baseline) 0))

(define (estimated-context-tokens messages usage)
  (define total (hash-try-get usage 'total_tokens))
  (define baseline (hash-try-get usage '_message_tokens))
  (if (and total (not baseline))
      total
      (+ (estimated-fixed-tokens messages usage)
         (estimated-message-tokens messages))))

(define (selected-compaction-needed? messages usage max-tokens)
  ((hash-ref (find-named compactor-registry selected-compactor) 'needed)
   messages usage max-tokens selected-compactor-config))

(define (start-selected-compaction messages max-tokens)
  ((hash-ref (find-named compactor-registry selected-compactor) 'start)
   messages max-tokens selected-compactor-config))

(define (complete-selected-compaction messages usage max-tokens events)
  ((hash-ref (find-named compactor-registry selected-compactor) 'complete)
   messages usage max-tokens events selected-compactor-config))

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
  (if (not (equal? selected-file-editor ""))
      (find-named file-editor-registry selected-file-editor))
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

    pub fn file_editor_tool_name(&mut self) -> Result<String> {
        eval_string(&mut self.vm, "(selected-file-editor-tool-name)")
    }

    pub fn composition_status(&mut self) -> Result<CompositionStatus> {
        let encoded = eval_string(&mut self.vm, "(value->jsexpr-string (composition-status))")?;
        serde_json::from_str(&encoded).context("decode composition status")
    }

    pub fn prepare_file_edit(
        &mut self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let encoded_arguments = serde_json::to_string(arguments)?;
        let encoded = eval_string(
            &mut self.vm,
            &format!(
                "(value->jsexpr-string (prepare-file-edit {} (string->jsexpr {})))",
                scheme_string(name),
                scheme_string(&encoded_arguments),
            ),
        )?;
        serde_json::from_str(&encoded).context("decode file edit preparation")
    }

    pub fn propose_file_edit(
        &mut self,
        name: &str,
        plan: &serde_json::Value,
        snapshots: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let encoded_plan = serde_json::to_string(plan)?;
        let encoded_snapshots = serde_json::to_string(snapshots)?;
        let encoded = eval_string(
            &mut self.vm,
            &format!(
                "(value->jsexpr-string (propose-file-edit {} (string->jsexpr {}) (string->jsexpr {})))",
                scheme_string(name),
                scheme_string(&encoded_plan),
                scheme_string(&encoded_snapshots),
            ),
        )?;
        serde_json::from_str(&encoded).context("decode proposed file changes")
    }

    pub fn complete_callable_tool(
        &mut self,
        implementation: &str,
        events: &[serde_json::Value],
    ) -> Result<serde_json::Value> {
        let encoded_events = serde_json::to_string(events)?;
        let encoded = eval_string(
            &mut self.vm,
            &format!(
                "(value->jsexpr-string (complete-callable-tool (find-named tool-implementation-registry {}) (string->jsexpr {})))",
                scheme_string(implementation),
                scheme_string(&encoded_events),
            ),
        )?;
        serde_json::from_str(&encoded).context("decode callable tool result")
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

pub fn composition_plugins(config: &Path) -> Result<Vec<String>> {
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
            (define (select-file-editor! _) #t)
            (define (register-model! _ __) #t)
            (define (unregister-model! _) #t)
            (define agent-instructions "")
            (define (set-agent-instructions! value)
              (set! agent-instructions value))
            (define (model-spec _) (hash))
            (define (callable-tool-for _ __) #f)
            (define (provider-arguments-for _ __) (hash))
            (define (start-callable-tool _ __) (hash))
            (define (provider-calls-for _ __) '())
            (define (provider-usage-for _ __) #f)
            (define (provider-preserved-items-for _ __) '())
            (define (provider-output-for _ __) "")
            (define (provider-message-phase-for _ __) #f)
            (define (selected-compaction-needed? _ __ ___) #f)
            (define (start-selected-compaction _ __) (hash))
            (define (complete-selected-compaction _ __ ___ ____) '())
            (define (provider-request _ __ ___ ____) (hash))
            (define (build-selected-prompt _ __ ___) (hash))
            (define (tools-for-model _) '())
            (define (runtime-config-value _ fallback) fallback)
            (define (estimated-message-tokens _) 0)
            (define (estimated-context-tokens _ __) 0)
            {}
        "#,
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
        .context("run Steel policy")?;
    let value = values.last().context("Steel policy returned no value")?;
    String::from_steelval(value).map_err(|error| anyhow::anyhow!(error.to_string()))
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
        let temp = tempfile::tempdir().unwrap();
        let provider = temp.path().join("openai.scm");
        let source = fs::read_to_string(root.join("policy/providers/openai.scm"))
            .unwrap()
            .replace(
                "'compaction_token_limit 244800",
                &format!("'compaction_token_limit {limit}"),
            );
        fs::write(&provider, source).unwrap();
        let mut sources = plugins(root);
        sources[1] = provider;
        Policy::load(&root.join("config.scm"), &sources).unwrap()
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
    fn compaction_recovers_from_unstructured_model_output() {
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
        assert_eq!(
            output.effects,
            vec![Effect::Finish {
                content: "Compaction complete.".into()
            }]
        );
        let state: serde_json::Value = serde_json::from_str(policy.state()).unwrap();
        assert_eq!(state["compactions"], 1.0);
        assert!(
            state["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("The model ignored the JSON request.")
        );
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
