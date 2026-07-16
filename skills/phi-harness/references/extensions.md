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
- Skills expose their name, description, and precedence-resolved `skill://NAME/SKILL.md` resource first. Read that file and its relative Markdown resources progressively with `read_file`; resource reads stay contained within the selected skill root. Plugins register a package-relative skill directory with `(register-skill! (hash 'path "skills/NAME"))`.

The official `context-management` plugin provides `context_mark`, `context_inspect`, and `context_compact`. They are generic focus-management tools, not planning-specific APIs. Item IDs and safe boundaries are runtime assigned; models select only IDs returned by `context_inspect`, never message offsets or arbitrary event ranges.

Provider-neutral prompts contain `instructions`, `messages`, and `tools`. A compactor may add `output_schema`; Responses-compatible providers map it to strict JSON-schema output.

Provider plugins may register default models. Because plugin entrypoints are evaluated before `config.scm`, configuration can customize those defaults with the same extension API:

- `(register-model! "provider" spec)` adds a model. If the qualified ID already exists, the later registration replaces it.
- `(unregister-model! "provider/model-id")` removes an inherited or previously configured model.

Keep complete model metadata in the registration spec: label, description, context and compaction limits, tool compatibility, reasoning options, and service tiers.

Prefer Steel for configurable behavior. Add Rust only for trusted effects, containment, durable state, transport, scheduling, or primitives Steel cannot safely provide.

The bundled `codex-patch` editor accepts locator text on an `@@` line or as a context-only hunk before a later changing hunk. Repeated plain update sections for one file run sequentially as one atomic edit. Each update must contain at least one syntactic change and must change file content or destination. Matching errors identify the file and hunk.

Plugin-specific operational instructions belong in plugin-registered skills. For example, read the `dynamic-workflows` skill before creating or troubleshooting workflows.

## Plugin workflow

```sh
phi plugin install URL --rev TAG_OR_COMMIT --path OPTIONAL_PATH
phi plugin check NAME
phi plugin list
phi update-plugins
```

Installation does not activate a plugin. Add `(load-plugin! "NAME")` to `~/.phi/config.scm`, then call `reload_config`. The old composition remains active if validation fails.

Official plugins are listed with versions and source paths in `official-plugins.json`. Fresh homes use the embedded snapshot without adding lock entries. `/update-plugins` and `phi update-plugins` resolve the latest official `main` revision and refresh all installed plugins from their recorded moving revisions.
