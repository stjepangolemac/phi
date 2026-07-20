# Phi

Phi is a small, configurable agent harness. Rust owns trusted mechanisms such as HTTP, secrets, filesystem/process capabilities, sessions, and the terminal UI. Sandboxed [Steel](https://github.com/mattwparas/steel) code owns providers, prompts, skills, compaction, commands, and the agent loop.

## Install and run

```sh
cargo install --path crates/phi-cli
phi
```

`phi` starts the TUI in the current directory. The first run initializes `~/.phi`. The bundled setup uses `gpt-5.6-luna` with low reasoning and the existing Codex login at `~/.codex/auth.json`.

Useful checks:

```sh
phi init
phi doctor
phi status
phi --json status
phi check-config
phi run "Reply with exactly: phi works"
phi --json run "Stream this response"
phi --allow-shell run "List files in this directory"
phi --yolo # run without approvals or filesystem boundaries
phi resume SESSION_ID "Continue"
```

New sessions are stored durably at `$PHI_HOME/sessions/<session-id>/` (normally `~/.phi/sessions/<session-id>/`). The flat home store contains the conversation's events and state plus exact configuration, loaded plugin sources, and installed plugin skill resources. The working directory and temporary Git worktree are metadata only. Existing workspace-local `.phi/sessions/` data is left untouched and is not migrated or resumed implicitly.

The shell tools run arbitrary commands through the user's shell, including pipelines and compound commands. Long-running commands yield a background session that survives model turns; the model can list sessions, poll them, or continue through stdin. Use `/ps` to inspect background processes and `/stop` to stop them. Use `/compact` to run the selected compactor immediately instead of waiting for the configured token threshold. PTYs are available for interactive programs. Tool approval is still required unless `--allow-shell` or `--yolo` is used. The bundled argument-aware policy intentionally still asks for destructive, local-mutating, remote-mutating, shell-composed, or ambiguous Git commands under `--allow-shell`; common simple reads such as `git status`, `git diff`, `git log`, and `git show` remain allowed. `--yolo` is the explicit bypass and also removes filesystem boundaries. OS sandboxing is intentionally not implemented yet.

Run `/keys` in the TUI for the complete keybinding reference and detailed input, cached-input, cache-write, and output token counters. The essentials are: `Enter` sends or steers the active turn, `Tab` queues the next turn, `Shift+Enter` or `Ctrl+Enter` inserts a newline, `Up/Down` reaches composer history at the input boundaries, `Shift+Up/Down` or `PageUp/PageDown` scrolls, and `Ctrl+C` cancels an active turn or quits when idle. `Esc` manages queued input before cancelling a turn; pickers use `Up/Down`, `Enter`, and `Esc`; approvals use `y`, `n`, or `Esc`. Ctrl+C during a slash command reports that cancellation is unavailable rather than silently ignoring the key.

## Configuration

Phi keeps behavior in Scheme and data in JSON:

```text
~/.phi/
  config.scm           # agent behavior, plugins, providers, tools, and composition
  config.json          # permissions, network origins, and secret handles
  state.json           # last selected model settings
  plugins.lock.json    # Git sources and exact commits
  plugins/             # immutable installed plugin revisions
  skills/              # manually copied personal skills
  sessions/            # durable flat conversation sessions, workflow runs, and plans
  builtins/<version>/  # official plugins embedded in this Phi build, plus system skills
```

`config.scm` is the sole mutable Scheme configuration root:

```scheme
(load-plugin! "responses")
(load-plugin! "openai")
(load-plugin! "openrouter")
(load-plugin! "openai-web-search")
(load-plugin! "openrouter-web-search")
(load-plugin! "skills")
(load-plugin! "context-management")
(load-plugin! "dynamic-workflows")
(load-plugin! "codex-patch")
(load-plugin! "simple-prompt")
(load-plugin! "compaction-structured")

(select-prompt-builder! "simple")
(select-file-editor! "codex-patch")
(configure-tool! "openai/hosted-web-search" (hash))
(configure-tool!
  "openai/callable-web-search"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"
        'search (hash)))
(configure-tool!
  "openrouter/hosted-web-search"
  (hash 'engine "native"))
(select-tool!
  "web_search"
  (list (hash 'prefer "same-route-hosted")
        (hash 'use "openai/callable-web-search")))
(select-compactor!
  "structured"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"
        'retain_messages 16
        'retain_token_limit 24000))
```

Tool approval may be tightened in `config.scm` with an optional Steel hook:

```scheme
(set-tool-approval-policy!
  (lambda (name arguments)
    (if (and (equal? name "exec_command")
             (equal? (hash-ref arguments 'cmd) "git push --force"))
        (hash 'decision "deny" 'detail "shell: git push --force")
        (hash 'decision "allow" 'detail name))))
```

The hook receives the model-facing tool name and its parsed JSON arguments and must return a hash containing `decision` (`"allow"`, `"ask"`, or `"deny"`) and a concise `detail` for approval frontends. Rust evaluates the CLI permission fallback and the Steel result, enforces the stricter decision, and performs the capability only after authorization. Thus a hook can attenuate `--allow-shell` or `--allow-write`, but cannot expand them; without a hook the existing flags remain the fallback. The trusted `--yolo` mode bypasses approval policy. Invalid hook output and hook failures stop the turn rather than executing the tool, while non-interactive modes return a tool error for `ask` or `deny` because no approval channel exists.

The selected file editor owns its model-facing format and matching logic in Steel. Rust supplies contained file snapshots, checks revisions, requests write approval, and persists the proposed changes. The bundled `codex-patch` editor exposes one `patch` tool for add, update, delete, and move operations. Update operations accept locator text directly on an `@@` line or in a context-only hunk before a later changing hunk; repeated plain update sections for one file run sequentially as one atomic edit, and every update must still change file content or destination. Approved reads and edits may target the workspace or Phi home; `--yolo` removes all filesystem boundaries.

There is no default provider. Providers register qualified model identities such as `openai/gpt-5.6-luna`; the selected model determines the provider. Use `/model` in the TUI to pick the model, reasoning, and provider-supported service tier.

Provider plugins may register a useful default model catalog. Configuration is evaluated after plugin entrypoints, so `config.scm` can customize that catalog: `register-model!` adds a model or replaces an existing model with the same qualified ID, while `unregister-model!` removes an inherited model. For example:

```scheme
(unregister-model! "openrouter/anthropic/claude-sonnet-4.6")
(register-model!
  "openrouter"
  (hash 'id "minimax/minimax-m3"
        'label "MiniMax M3"
        'description "MiniMax M3 through OpenRouter."
        'context_window 1000000
        'compaction_token_limit 180000
        'strict_json_schema_capable #f
        'function_tools #t
        'hosted_tools (list "openrouter/hosted-web-search")
        'reasoning (list (hash 'id "high" 'description "Greater reasoning depth."))
        'default_reasoning "high"
        'service_tiers '()
        'default_service_tier ""))
```

Provider-neutral prompts contain `instructions`, `messages`, and `tools`; compactors may also attach `output_schema`. Models verified to support strict schema decoding register `strict_json_schema_capable` as true. Structured compaction uses native schemas for those models, otherwise prompts for JSON, validates the result, and makes up to four repair attempts.

Responses-compatible provider stream rules associate output deltas with each output message's phase, emitting `commentary_delta` for concise user-visible progress and retaining `model_delta` for final-answer text. Hosted-tool lifecycle events remain separate. The bundled OpenAI provider disables readable reasoning summaries while preserving the selected reasoning effort and opaque encrypted reasoning continuity. Commentary is persisted and replayed as ordinary assistant output with `phase: "commentary"`; legacy persisted reasoning-summary blocks remain readable, and encrypted reasoning is never rendered or decrypted.

The official `context-management` plugin exposes four generic model-facing tools. `context_mark` closes the current raw span and starts a labeled span at a tool-safe boundary. `context_inspect` reports provider-anchored or estimated usage, fixed prompt tokens, ordered active `S*` raw and `C*` summary items, and selective-compaction jobs. `context_compact` validates and reserves one or more ordered, adjacent, closed items, queues a background summarization job, and immediately returns its independent `J*` ID and `queued` status. Providers may return multiple context-tool calls in one response; Phi preserves each call ID and result, dispatches calls in provider order across ordinary-tool barriers, and allows disjoint jobs to run concurrently while invalid or overlapping calls receive only their own error result. Completed summaries are applied only at agent-loop boundaries, never by rewriting an in-flight provider prompt. `context_wait` waits for selected job IDs, or accepts `null` to snapshot all jobs pending when the call begins; terminal jobs return immediately and unknown IDs are independent errors. Jobs retain `queued`, `running`, `applied`, `failed`, `cancelled`, or `stale` status for inspection. Failed, cancelled, stale, overlapping, and full-compaction interactions preserve the active context, while successful jobs replace their reserved items with one summary retaining direct and nested provenance. The fixed prompt and current open span are never selectable. These boundaries are independent of planning and can represent investigation, implementation, debugging, review, user-directed scope changes, or any other focus shift. When selective compaction is available, model-facing pressure notices are advisory at 25%, encouraged at 50%, and high priority at 75%; a jump emits only the highest newly crossed notice, and compaction resets the next applicable threshold. Automatic threshold compaction and `/compact` remain the safety net when no markers exist and supersede pending selective jobs. Immutable session events retain the complete raw history while only the active model-facing projection is reduced.

After changing `config.scm` or `config.json`, use `/reload`. The agent can call `reload_config` after reconfiguring itself. Reload validates the live composition, replaces the current session snapshot, and updates the catalog without discarding the conversation.

Installed and official plugin files are implementation packages, not runtime configuration. Phi embeds the official plugin snapshot and copies it into `~/.phi/builtins/<version>/` during home initialization, so the build checkout does not need to remain available. Configure providers, models, tools, prompts, compaction, and agent behavior only in the `config.scm` reported by `phi --json status`. If a plugin lacks a needed setting, extend its configuration interface instead of patching an installed copy.

Tool implementations declare model compatibility. The configuration above prefers search hosted by the selected model's provider route, then explicitly falls back to a separate OpenAI search request. Put an OpenRouter key in `~/.phi/secrets/openrouter.json` as `{"api_key":"..."}` before selecting an `openrouter/...` model.

Local tool execution is also policy-routed. Steel attaches one closed protocol effect to each returned tool call, and Rust dispatches the trusted mechanism without branching on the model-facing tool name. The supported local modes are `capability`, `managed_process` (`execute`, `write_stdin`, `list`, or `terminate`), `file_edit`, `workflow` (`launch`, `output`, or `stop`), and `reload_config`; callable tools use `http`. Runtime-provided capabilities and process tools receive routes automatically, the selected file editor receives `file_edit`, and bundled workflow tools declare their actions in the plugin. A plugin can declare the same contract with `(register-tool! builder (hash 'mode "..." ...))`, so a new alias backed by an existing effect needs no Rust agent-loop change. Unknown modes or actions are rejected while decoding the policy effect. The legacy one-argument `(register-tool! builder)` form remains accepted for persisted and external plugins: historical built-in names are migrated to their typed routes and every other registration keeps the former direct-capability behavior. Sessions pinned to the previous config may also emit `mode: "direct"`; runtime converts that representation once, including the pinned selected editor, before entering the typed dispatcher.

## Skills

Copy standard `SKILL.md` directories into `~/.phi/skills/` for personal use or `.phi/skills/` for one workspace:

```text
~/.phi/skills/review/
  SKILL.md
  references/
```

Every installed plugin may provide skills conventionally under `skills/NAME/SKILL.md`; no Scheme registration or plugin activation is required. Plugin skills have the lowest precedence, followed by personal and workspace skills; protected Phi system skills have the highest precedence. Duplicate skill names from two installed plugins are rejected with both plugin names rather than selected by incidental order. Phi initially exposes each resolved skill's name, description, and stable `skill://NAME/SKILL.md` resource. The normal `read_file` tool reads that file and referenced resources such as `skill://NAME/references/details.md`; every such read is contained within the precedence-selected skill root without exposing Phi's installation or plugin-cache layout. Sessions snapshot installed plugin skill packages, so updates and removals affect new or reloaded sessions while an existing session remains internally consistent. Use `/skills` to list discovered skills or mention `$skill-name` to request one explicitly.

Phi also bundles an authoritative `phi-harness` skill describing its architecture, configuration, extension points, and operations. The agent reads it before inspecting or reconfiguring the harness. Use `phi status` for the human-readable active composition or `phi --json status` for machine-readable output.

### Local planning

The bundled `planning` skill uses numbered Markdown files under the current durable session, for example `$PHI_HOME/sessions/<session-id>/plans/0001-session-storage.md`. The `create_plan` tool allocates names atomically and requires write approval. Multiple plans may coexist without an active-plan pointer, archive, index, or database. Each plan contains one goal, a separate acceptance-criteria checklist, one plan-wide `writing`, `executing`, or `done` stage, a flat task checklist, blockers, and resume notes. Pending, current, and completed tasks use `[ ]`, `[>]`, and `[x]`; exactly one task is current during execution, while writing and done plans have no current task.

During writing, the agent gathers relevant user and workspace context and may perform non-mutating discovery, but it does not implement anything until the user explicitly approves the plan. After approval it executes autonomously, updating the plan with its best judgment and pausing only for a genuine blocker, required user decision, or permission or safety boundary. A plan becomes done only after every task and acceptance criterion is complete and nothing remains to do. At meaningful checkpoints the agent updates the plan, and a compacted conversation resumes by reading it. Completed plans remain in the durable session directory. New plans no longer use workspace `.phi/PLAN.md`; existing files are left untouched. This skill-first workflow uses ordinary file tools rather than a `/plan` command suite and takes design inspiration from the planning workflows in OpenAI Codex and Claude Code.

## Dynamic workflows

The official `dynamic-workflows` plugin exposes background `Workflow`, `TaskOutput`, and `TaskStop` tools. Reusable workflow definitions are JavaScript modules. Global definitions live in `~/.phi/workflows/`, workspace-specific definitions in `.phi/workflows/`, and packaged definitions in the `workflows/` directory of a loaded plugin. Name-only calls discover them in this order:

```text
~/.phi/workflows/NAME.js
.phi/workflows/NAME.js
LOADED_PLUGIN/workflows/NAME.js
```

`Workflow` also accepts an optional exact `path` to select a same-named definition regardless of discovery precedence. Relative paths resolve from the current workspace; absolute paths are accepted too. Exact paths must name a regular `.js` file contained in the global, current-workspace, or a loaded-plugin workflow root. Parent traversal, escaping symlinks, unloaded-plugin paths, and paths outside those roots are rejected. The module's `meta.name` must equal the requested `name`.

Create project-specific definitions in `.phi/workflows/`. Copy or promote one to `~/.phi/workflows/` only when the user explicitly asks to make it global.

Each module exports `meta` and a default async function. The `phi:workflow` module supplies `agent`, `parallel`, `batch`, `pipeline`, `phase`, `log`, and `budget`:

```js
import { agent, batch, parallel, phase } from "phi:workflow"

export const meta = {
  name: "review",
  description: "Review a change from several perspectives.",
  inputSchema: {
    type: "object",
    properties: {
      files: { type: "array", items: { type: "string" } },
      mode: { enum: ["batch", "parallel"] }
    },
    required: ["files", "mode"],
    additionalProperties: false
  }
}

export default async function ({ args }) {
  phase("Review")
  const reviews = args.files.map((file, index) => () => agent(
    `Review file ${index + 1}:\n${file}`,
    { label: `review-${index + 1}` }
  ))
  return args.mode === "batch"
    ? batch(reviews, { size: 3 })
    : parallel(reviews, { concurrency: 3 })
}
```

`meta.inputSchema` is optional. When present, it is validated when the workflow is discovered and `Workflow.args` is validated before Phi creates a task directory, launches the runner, or creates a child session. Validation failures report JSON Pointer-style instance and schema paths. The `Workflow` tool description lists each name-discovered workflow's description and schema; a successful launch also returns `description` and `input_schema`, identically for name and exact-path selection. Without `inputSchema`, `args` remains an arbitrary JSON value for backward compatibility.

Input schemas support boolean schemas and the common draft 2020-12/draft-07 subset: `type`, `enum`, `const`, `properties`, `required`, `additionalProperties`, `items`, string/array/object size limits, `pattern`, numeric limits, `multipleOf`, `uniqueItems`, and `allOf`/`anyOf`/`oneOf`/`not`, plus standard annotation keywords. Unsupported features such as `$ref`, `$defs`, conditionals, tuple/prefix items, dependencies, and formats are rejected with the unsupported keyword's schema path rather than ignored.

`parallel(tasks, { concurrency })` continuously fills up to the requested number of task slots while preserving result order. `batch(tasks, { size })` runs fixed-size waves through `parallel()` and waits for each wave before starting the next. Tasks are functions so their work does not start before the scheduler invokes them. Both APIs remain subject to the workflow runtime's global agent concurrency limit.

`agent(prompt, { label?, schema?, branch?, branch_off? })` allocates a durable child session in the same Phi home before starting a one-shot Phi child. Child agents run with `--yolo`; workflows are therefore trusted local code. A schema requests strict JSON-schema output and makes `agent()` return the parsed JSON value. The initial runtime limits are 8 concurrent agents, 32 agents total per workflow, and 60 minutes per workflow. Workflow runs are distinct from definitions: task files, progress, logs, results, managed-worktree ownership metadata, and child relationships live under `$PHI_HOME/sessions/<parent-session-id>/workflows/<task-id>/`. Every launch receives a unique task ID and directory, including launches of same-named definitions. Each child has a separate flat `$PHI_HOME/sessions/<child-session-id>/` entry recording its parent session, workflow task, agent label, branch, workspace, and actual worktree path. Background tasks live for the duration of the parent Phi process and are cancelled when it exits, but their run records and child sessions remain durable.

For a single focused child, the plugin bundles `delegate`. It takes a required `prompt` plus optional `options` matching the currently supported `agent()` options (`label`, `schema`, `branch`, and `branch_off`) and returns the child result directly:

```json
{
  "name": "delegate",
  "args": {
    "prompt": "Summarize the current diff.",
    "options": {
      "label": "summary",
      "schema": {
        "type": "object",
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"],
        "additionalProperties": false
      }
    }
  }
}
```

Launch it through `Workflow`, inspect or wait through `TaskOutput`, and cancel through `TaskStop`. Because it is an ordinary workflow using one ordinary `agent()` call, it uses the same budgets, durable child/task records, terminal failure bookkeeping, and cleanup as every other workflow.

For isolated edits, set `branch` to a workflow-local logical name. Phi creates a task-owned `phi/...` branch and deterministic temporary Git worktree from the workflow's launch commit, then starts that child in the corresponding repository-relative subdirectory. `branch_off` is valid only with `branch` and can name either an external Git ref or a completed managed logical branch. Integration agents receive the logical-to-actual branch map so they can merge completed managed work. Branched agents should commit and return commit hashes through schemas. All managed worktrees and refs are deleted on every workflow exit, including errors, timeouts, cancellation, and stale-task cleanup; wanted commits must therefore be promoted to a non-managed ref before exit, normally by a final explicit unbranched `agent()` call. Cleanup only operates on paths and refs recorded as created by that workflow task and never adopts pre-existing branches or worktrees.

`TaskOutput` accepts required nullable `wait_ms`: `null` waits up to the 15-second default, `0` checks immediately, and an integer selects a wait up to 300 seconds. Its structured summary reports the active phase, latest workflow log, and running, completed, and failed agent counts so the TUI can show useful progress without dumping internal task paths or raw progress events.

The public child-agent transport is one-request, line-framed JSON-RPC over stdio:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"agent.run","params":{"prompt":"Reply with ok","schema":null}}' | phi --workspace . --yolo rpc
```

Phi emits `agent.event` notifications followed by a result containing `value` and `sessionId`, or a JSON-RPC error.

## Plugins

A plugin is a Git-backed convention-based directory package:

```text
plugins/example/
  plugin.scm
  skills/
    example/
      SKILL.md
  support/            # arbitrary package-relative files are allowed
```

`plugin.scm` is the mandatory fixed entrypoint. Install from a tag or commit with an explicit lowercase installation name, optionally selecting a package within a larger repository:

```sh
phi plugin install example https://github.com/example/phi-plugins --rev v0.1.0 --path plugins/example
phi plugin list
phi plugin check example
phi plugin update example --rev NEW_TAG_OR_COMMIT
phi plugin sync
phi plugin remove example
phi update-plugins
```

Installation records the lock-owned name, URL, requested revision, resolved commit, and source path but does not activate the plugin. Add `(load-plugin! "example")` to `~/.phi/config.scm` and compose its registered behavior there. Conventional skills are available even while the plugin is unloaded.

Plugins have no package-level type and are never grouped by provider, tool, prompt, or compactor type. Their entrypoints may register providers and models, tools, prompt builders, compactors, file editors, or slash commands. Package trees and conventional skill resources reject escaping paths and symlinks.

Fresh Phi homes have an empty `plugins.lock.json`; the official package identities and source paths in `official-plugins.json` are available from the embedded build snapshot. On startup Phi compares that snapshot and installed Git plugins with their configured sources. When updates are available the TUI suggests `/update-plugins`. Both `/update-plugins` and `phi update-plugins` resolve the official `latest` channel from the public repository's `main` branch, install exact directory packages, and retain those commits in the lock file. Existing sessions keep their snapshotted plugin code and skills until `/reload` or a new conversation.

## Optional features

These are possible extensions, not committed scope. Do not implement them without explicit operator approval:

- file review
- layered project instructions
- sandbox and permission modes
- turn steering and structured questions
- richer session management
- MCP integration
- checkpoints and rewind
- lifecycle hooks
- worktrees
- durable memory
- multimodal, editor, and browser context

## Workspace

```text
crates/phi-cli       installed `phi` binary
crates/phi-core      trusted capabilities, sessions, plugins, and home layout
crates/phi-protocol  provider-neutral events and effects
crates/phi-runtime   frontend-neutral agent loop
crates/phi-steel     Scheme composition and policy VM
crates/phi-tui       Ratatui frontend library
plugins/             bundled convention-based plugin packages
```

Validate changes with:

```sh
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
