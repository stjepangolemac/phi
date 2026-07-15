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

Provider plugins may register default models. Because plugin entrypoints are evaluated before `config.scm`, configuration can customize those defaults with the same extension API:

- `(register-model! "provider" spec)` adds a model. If the qualified ID already exists, the later registration replaces it.
- `(unregister-model! "provider/model-id")` removes an inherited or previously configured model.

Keep complete model metadata in the registration spec: label, description, context and compaction limits, tool compatibility, reasoning options, and service tiers.

Prefer Steel for configurable behavior. Add Rust only for trusted effects, containment, durable state, transport, scheduling, or primitives Steel cannot safely provide.

## Plugin workflow

```sh
phi plugin install URL --rev TAG_OR_COMMIT --path OPTIONAL_PATH
phi plugin check NAME
phi plugin list
```

Installation does not activate a plugin. Add `(load-plugin! "NAME")` to `~/.phi/config.scm`, then call `reload_config`. The old composition remains active if validation fails.
