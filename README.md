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

Sessions and their exact configuration/plugin sources are stored under `.phi/sessions/` in the working directory.

The shell tools run arbitrary commands through the user's shell, including pipelines and compound commands. Long-running commands yield a background session that survives model turns; the model can list sessions, poll them, or continue through stdin. Use `/ps` to inspect background processes and `/stop` to stop them. Use `/compact` to run the selected compactor immediately instead of waiting for the configured token threshold. PTYs are available for interactive programs. Tool approval is still required unless `--allow-shell` or `--yolo` is used. `--yolo` also lets file reads and edits target paths outside the workspace. OS sandboxing is intentionally not implemented yet.

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

Responses-compatible provider stream rules normalize answer text as `model_delta`, provider-generated reasoning summaries as `reasoning_summary_delta`, and hosted-tool lifecycle events separately. The bundled OpenAI provider requests concise summaries while preserving the selected reasoning effort and encrypted reasoning state; the TUI labels summaries distinctly and never mixes them into the assistant answer.

The official `context-management` plugin exposes three generic model-facing tools. `context_mark` closes the current raw span and starts a labeled span at a tool-safe boundary. `context_inspect` reports provider-anchored or estimated usage, fixed prompt tokens, and the ordered active `S*` raw and `C*` summary items. `context_compact` replaces one or more ordered, adjacent, closed items with one summary while retaining direct and nested provenance. The fixed prompt and current open span are never selectable. Context tools are dispatched serially so every exposed call reaches the stateful policy handler for that turn. These boundaries are independent of planning and can represent investigation, implementation, debugging, review, user-directed scope changes, or any other focus shift. Automatic threshold compaction and `/compact` remain the safety net when no markers exist, and immutable session events retain the complete raw history while only the active model-facing projection is reduced.

After changing `config.scm` or `config.json`, use `/reload`. The agent can call `reload_config` after reconfiguring itself. Reload validates the live composition, replaces the current session snapshot, and updates the catalog without discarding the conversation.

Installed and official plugin files are implementation packages, not runtime configuration. Phi embeds the official plugin snapshot and copies it into `~/.phi/builtins/<version>/` during home initialization, so the build checkout does not need to remain available. Configure providers, models, tools, prompts, compaction, and agent behavior only in the `config.scm` reported by `phi --json status`. If a plugin lacks a needed setting, extend its configuration interface instead of patching an installed copy.

Tool implementations declare model compatibility. The configuration above prefers search hosted by the selected model's provider route, then explicitly falls back to a separate OpenAI search request. Put an OpenRouter key in `~/.phi/secrets/openrouter.json` as `{"api_key":"..."}` before selecting an `openrouter/...` model.

## Skills

Copy standard `SKILL.md` directories into `~/.phi/skills/` for personal use or `.phi/skills/` for one workspace:

```text
~/.phi/skills/review/
  SKILL.md
  references/
```

Loaded plugins can register package-relative skill directories with `(register-skill! (hash 'path "skills/NAME"))`. Plugin skills have the lowest precedence, followed by personal and workspace skills; protected Phi system skills have the highest precedence. Phi initially exposes each resolved skill's name, description, and stable `skill://NAME/SKILL.md` resource. The normal `read_file` tool reads that file and referenced resources such as `skill://NAME/references/details.md`; every such read is contained within the precedence-selected skill root without exposing Phi's installation or plugin-cache layout. Use `/skills` to list discovered skills or mention `$skill-name` to request one explicitly.

Phi also bundles an authoritative `phi-harness` skill describing its architecture, configuration, extension points, and operations. The agent reads it before inspecting or reconfiguring the harness. Use `phi status` for the human-readable active composition or `phi --json status` for machine-readable output.

### Local planning

The bundled `planning` skill uses `.phi/PLAN.md` as lightweight persistent state for nontrivial, multi-step work. The plan is human-readable Markdown: one goal, a separate acceptance-criteria checklist, one plan-wide `writing`, `executing`, or `done` stage, a flat task checklist, blockers, and resume notes. Pending, current, and completed tasks use `[ ]`, `[>]`, and `[x]`; exactly one task is current during execution, while writing and done plans have no current task. Tasks do not have independent stages, stable IDs, subtasks, milestones, or dependency graphs.

During writing, the agent gathers relevant user and workspace context and may perform non-mutating discovery, but it does not implement anything until the user explicitly approves the plan. After approval it executes autonomously, updating the plan with its best judgment and pausing only for a genuine blocker, required user decision, or permission or safety boundary. A plan becomes done only after every task and acceptance criterion is complete and nothing remains to do. At meaningful checkpoints the agent updates the plan, and a new session or compacted conversation resumes by reading it. In a Git workspace the agent records the plan's repository-relative path in the repository-local `.git/info/exclude`, never in a shared `.gitignore`; completed plans remain on disk and are not committed. This skill-first workflow uses ordinary file tools rather than a `/plan` command suite and takes design inspiration from the planning workflows in OpenAI Codex and Claude Code.

## Dynamic workflows

The official `dynamic-workflows` plugin exposes background `Workflow`, `TaskOutput`, and `TaskStop` tools. Workflows are named JavaScript modules discovered in this order:

```text
.phi/workflows/NAME.js
~/.phi/workflows/NAME.js
LOADED_PLUGIN/workflows/NAME.js
```

Each module exports `meta` and a default async function. The `phi:workflow` module supplies `agent`, `parallel`, `batch`, `pipeline`, `phase`, `log`, and `budget`:

```js
import { agent, batch, parallel, phase } from "phi:workflow"

export const meta = {
  name: "review",
  description: "Review a change from several perspectives."
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

`parallel(tasks, { concurrency })` continuously fills up to the requested number of task slots while preserving result order. `batch(tasks, { size })` runs fixed-size waves through `parallel()` and waits for each wave before starting the next. Tasks are functions so their work does not start before the scheduler invokes them. Both APIs remain subject to the workflow runtime's global agent concurrency limit.

`agent(prompt, { label?, schema?, branch?, branch_off? })` starts a fresh one-shot Phi child in the same workspace and Phi home. Child agents run with `--yolo`; workflows are therefore trusted local code. A schema requests strict JSON-schema output and makes `agent()` return the parsed JSON value. The initial runtime limits are 8 concurrent agents, 32 agents total per workflow, and 60 minutes per workflow. Workflow task files, progress, logs, results, and managed-worktree ownership metadata live under the parent session's `workflows/tasks/` directory. Background tasks live for the duration of the parent Phi process and are cancelled when it exits.

For isolated edits, set `branch` to a workflow-local logical name. Phi creates a task-owned `phi/...` branch and deterministic temporary Git worktree from the workflow's launch commit, then starts that child in the corresponding repository-relative subdirectory. `branch_off` is valid only with `branch` and can name either an external Git ref or a completed managed logical branch. Integration agents receive the logical-to-actual branch map so they can merge completed managed work. Branched agents should commit and return commit hashes through schemas. All managed worktrees and refs are deleted on every workflow exit, including errors, timeouts, cancellation, and stale-task cleanup; wanted commits must therefore be promoted to a non-managed ref before exit, normally by a final explicit unbranched `agent()` call. Cleanup only operates on paths and refs recorded as created by that workflow task and never adopts pre-existing branches or worktrees.

`TaskOutput` accepts required nullable `wait_ms`: `null` waits up to the 15-second default, `0` checks immediately, and an integer selects a wait up to 300 seconds. Its structured summary reports the active phase, latest workflow log, and running, completed, and failed agent counts so the TUI can show useful progress without dumping internal task paths or raw progress events.

The public child-agent transport is one-request, line-framed JSON-RPC over stdio:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"agent.run","params":{"prompt":"Reply with ok","schema":null}}' | phi --workspace . --yolo rpc
```

Phi emits `agent.event` notifications followed by a result containing `value` and `sessionId`, or a JSON-RPC error.

## Plugins

A plugin is a Git-backed directory containing `plugin.json` and a Steel entrypoint:

```json
{
  "name": "example",
  "version": "0.1.0",
  "entrypoint": "main.scm"
}
```

Install from a tag or commit, optionally selecting a plugin within a larger repository:

```sh
phi plugin install https://github.com/example/phi-plugins --rev v0.1.0 --path plugins/example
phi plugin list
phi plugin check example
phi plugin update example --rev NEW_TAG_OR_COMMIT
phi plugin sync
phi plugin remove example
phi update-plugins
```

Installation records the resolved commit but does not activate the plugin. Add `(load-plugin! "example")` to `~/.phi/config.scm` and compose its registered behavior there.

Plugins have no package-level type. Their entrypoints may register providers and models, tools, skills, prompt builders, compactors, file editors, or slash commands. Registered skill paths are contained within the plugin package. The registration functions enforce the contract of each extension point.

Fresh Phi homes have an empty `plugins.lock.json`; the official plugins listed with versions in `official-plugins.json` are available from the embedded build snapshot. On startup Phi compares that snapshot and installed Git plugins with their configured sources. When updates are available the TUI suggests `/update-plugins`. Both `/update-plugins` and `phi update-plugins` resolve the official `latest` channel from the public repository's `main` branch, install exact commits, and retain those commits in the lock file. Existing sessions keep their snapshotted plugin sources until `/reload` or a new conversation.

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
policy/              bundled Scheme sources
```

Validate changes with:

```sh
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
