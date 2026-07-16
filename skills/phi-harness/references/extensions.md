# Extensions

Plugins are Git-backed directories with `plugin.json` and a Steel entrypoint. Package-level plugin types do not exist; entrypoints register behavior through explicit extension points.

`~/.phi/config.scm` is the composition root. It loads plugins, configures tools, and selects implementations.

Add model registrations and all provider or tool configuration to this composition root. Installed and bundled plugin files are immutable implementation code and must not be edited for runtime tweaking.

## Extension points

- Providers register qualified models such as `openai/gpt-5.6-luna`.
- Prompt builders assemble provider-neutral instructions, messages, and tools.
- Compactors decide when and how to summarize context.
- File editors define the model-facing edit format and matching policy; Rust verifies revisions and writes files.
- Hosted and callable tools declare compatible provider/model routes.
- Slash commands register local user operations.
- Skills expose metadata first and load Markdown resources progressively.

Provider-neutral prompts contain `instructions`, `messages`, and `tools`. A compactor may add `output_schema`; Responses-compatible providers map it to strict JSON-schema output.

Provider plugins may register default models. Because plugin entrypoints are evaluated before `config.scm`, configuration can customize those defaults with the same extension API:

- `(register-model! "provider" spec)` adds a model. If the qualified ID already exists, the later registration replaces it.
- `(unregister-model! "provider/model-id")` removes an inherited or previously configured model.

Keep complete model metadata in the registration spec: label, description, context and compaction limits, tool compatibility, reasoning options, and service tiers.

Prefer Steel for configurable behavior. Add Rust only for trusted effects, containment, durable state, transport, scheduling, or primitives Steel cannot safely provide.

## Dynamic workflows

The official `dynamic-workflows` plugin keeps orchestration in named JavaScript modules while Rust owns background process lifecycle and the public one-shot agent transport. Workflows are discovered from `.phi/workflows/NAME.js`, `~/.phi/workflows/NAME.js`, then `workflows/NAME.js` in loaded plugins.

Workflow modules export `meta` with `name` and `description`, plus a default async `({ args }) => value` function. They may import `agent`, `parallel`, `batch`, `pipeline`, `phase`, `log`, and `budget` from `phi:workflow`. `parallel(tasks, { concurrency? })` runs task functions with an optional continuously replenished concurrency limit. `batch(tasks, { size })` runs fixed-size waves and waits for each wave before starting the next. `agent(prompt, { label?, schema? })` launches a fresh `phi --workspace WORKSPACE --yolo rpc` child. Limits are 8 concurrent agents, 32 total agents, and 60 minutes. The model launches and manages workflows through `Workflow`, `TaskOutput`, and `TaskStop`; task state is stored under the parent session.

`phi rpc` accepts one line-framed JSON-RPC 2.0 `agent.run` request on stdin. Params contain `prompt` and optional `schema`; stdout contains `agent.event` notifications followed by a result with `value` and `sessionId`, or a JSON-RPC error. This interface is public and versioned through Phi releases.

## Plugin workflow

```sh
phi plugin install URL --rev TAG_OR_COMMIT --path OPTIONAL_PATH
phi plugin check NAME
phi plugin list
```

Installation does not activate a plugin. Add `(load-plugin! "NAME")` to `~/.phi/config.scm`, then call `reload_config`. The old composition remains active if validation fails.
