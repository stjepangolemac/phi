# Extensions

Plugins are Git-backed directory packages named explicitly at installation. Every package has the fixed `plugin.scm` entrypoint and may contain arbitrary package-relative support files. Package-level plugin types and configurable entrypoints do not exist; entrypoints register behavior through explicit extension points.

`~/.phi/config.scm` is the composition root. It loads plugins, configures tools, and selects implementations.

Add model registrations and all provider or tool configuration to this composition root. Installed and bundled plugin files are immutable implementation code and must not be edited for runtime tweaking.

## Extension points

- Providers register qualified models such as `openai/gpt-5.6-luna`.
- Prompt builders assemble provider-neutral instructions, messages, and tools.
- Compactors decide when and how to summarize context.
- File editors define the model-facing edit format and matching policy; Rust verifies revisions and writes files.
- Hosted and callable tools declare compatible provider/model routes.
- Slash commands register local user operations.
- Skills expose their name, description, and precedence-resolved `skill://NAME/SKILL.md` resource first. Read that file and its relative Markdown resources progressively with `read_file`; resource reads stay contained within the selected skill root. Every installed plugin may provide conventional `skills/NAME/SKILL.md` resources without Scheme registration or activation.

The official `context-management` plugin provides `context_mark`, `context_inspect`, `context_compact`, and `context_wait`. They are generic focus-management tools, not planning-specific APIs. Item, summary, and job IDs and safe boundaries are runtime assigned; models select only IDs returned by `context_inspect` or `context_compact`, never message offsets or arbitrary event ranges. `context_compact` is asynchronous: it returns a queued `J*` job immediately, reserves the selected items while pending, and applies a successful summary only at an agent-loop boundary. `context_wait` accepts `job_ids`, with `null` snapshotting every job pending at call time; completed jobs return immediately, and unknown IDs fail clearly. Extensions must preserve active context on failed, cancelled, stale, overlapping, or superseded jobs and must not mutate provider prompts already in flight.

Provider-neutral prompts contain `instructions`, `messages`, and `tools`. A compactor may add `output_schema`; Responses-compatible providers map it to strict JSON-schema output.

Provider stream rules use provider-neutral emit names. Responses-compatible providers capture each output message's phase with `output_phase` and emit its text with `output_delta`; both rules use the same `key` JSON pointer, normally `/output_index`, so the runtime can associate concurrent message items. The runtime exposes commentary as `commentary_delta` and retains `model_delta` for final-answer text. Use `tool_started`/`tool_completed` for hosted-tool lifecycle events. A provider's final registration callback returns its ordered normalized assistant messages; legacy callbacks that return one phase remain accepted. Commentary is ordinary assistant output with `phase: "commentary"`, persists across tool turns, and is replayed in transcript order. The bundled OpenAI provider disables readable reasoning summaries while preserving opaque encrypted reasoning items independently. Continue accepting legacy persisted reasoning-summary blocks, but do not render or attempt to decrypt encrypted reasoning state.

Provider plugins may register default models. Because plugin entrypoints are evaluated before `config.scm`, configuration can customize those defaults with the same extension API:

- `(register-model! "provider" spec)` adds a model. If the qualified ID already exists, the later registration replaces it.
- `(unregister-model! "provider/model-id")` removes an inherited or previously configured model.

Keep complete model metadata in the registration spec: label, description, context and compaction limits, tool compatibility, reasoning options, and service tiers.

Prefer Steel for configurable behavior. Add Rust only for trusted effects, containment, durable state, transport, scheduling, or primitives Steel cannot safely provide.

The bundled `codex-patch` editor accepts locator text on an `@@` line or as a context-only hunk before a later changing hunk. Repeated plain update sections for one file run sequentially as one atomic edit. Each update must contain at least one syntactic change and must change file content or destination. Matching errors identify the file and hunk.

Plugin-specific operational instructions belong in conventional plugin skills. For example, read the `dynamic-workflows` skill before creating or troubleshooting workflows.

The official dynamic-workflows extension discovers name-only definitions globally from `$PHI_HOME/workflows/`, then from workspace `.phi/workflows/`, then from loaded plugins. Its `Workflow` tool can instead select an exact regular `.js` definition with `path`, provided the resolved path remains in one of those loaded roots and `meta.name` matches the requested name. Relative paths resolve from the workspace. Modules may declare optional `meta.inputSchema`; Phi exposes name-discovered schemas in the tool description, returns `description` and `input_schema` for both name and exact-path launches, and validates args before creating the task or runner. Missing schemas preserve arbitrary JSON args. The supported draft 2020-12/draft-07 subset covers boolean schemas, primitive types, enums/constants, object properties, homogeneous arrays, common limits, patterns, and combinators; unsupported keywords such as `$ref`, conditionals, tuple items, dependencies, and formats fail with JSON Pointer-style schema paths. Workflow runs remain unique directories under the durable parent session at `$PHI_HOME/sessions/<session-id>/workflows/<task-id>/`; they are not stored with reusable definitions. Every child agent has its own flat durable home session with parent, task, label, branch, workspace, and worktree relationship metadata.

The extension bundles `delegate` for one focused durable child. Its required `prompt` and optional `options` object expose the currently supported `agent()` label, structured-output schema, branch, and branch-off controls while retaining normal `Workflow`, `TaskOutput`, and `TaskStop` lifecycle behavior.

## Plugin workflow

```sh
phi plugin install NAME URL --rev TAG_OR_COMMIT --path OPTIONAL_PATH
phi plugin check NAME
phi plugin list
phi update-plugins
```

Installation does not activate a plugin. Add `(load-plugin! "NAME")` to `~/.phi/config.scm`, then call `reload_config`. The old composition remains active if validation fails.

Official plugin identities and package source paths are listed in `official-plugins.json`. Fresh homes use the embedded snapshot without adding lock entries. `/update-plugins` and `phi update-plugins` resolve the latest official `main` revision and refresh all installed plugins from their recorded moving revisions. New or reloaded sessions see updated conventional skills; existing sessions read their pinned copies.
