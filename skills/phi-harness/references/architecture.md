# Architecture

Phi is a Rust-hosted agent harness whose behavior is composed in sandboxed Steel.

## Ownership

- `phi-cli`: installed `phi` binary and non-TUI commands.
- `phi-core`: trusted HTTP, secrets, filesystem, processes, sessions, plugins, skills, and policy storage.
- `phi-protocol`: provider-neutral events, effects, tools, commands, and model metadata.
- `phi-runtime`: frontend-neutral agent loop and active composition.
- `phi-steel`: Steel VM, registries, validation, and policy execution.
- `phi-tui`: Ratatui frontend and transcript rendering.
- `plugins/`: bundled convention-based plugin packages.

Rust owns mechanisms that require containment or reliability. Steel owns provider behavior, prompt assembly, tool routing, compaction policy, editor formats, commands, and the agent loop.

## Turn lifecycle

1. Resolve `config.scm` and the plugin packages it loads.
2. Snapshot that composition and every installed plugin's conventional skills into a new durable session under `$PHI_HOME/sessions/<session-id>/`.
3. Build a provider-neutral prompt from instructions, messages, and compatible tools.
4. Let the selected provider plugin build and stream the request.
5. Execute returned tool calls through Rust capabilities or configured callable tools.
6. Feed results back until the policy finishes the turn.
7. Compact after a completed model/tool cycle when the selected model threshold is crossed, immediately when the user runs `/compact`, or selectively when the model compacts closed context items.

## Active context

The active model-facing context is an ordered reduction over immutable session history. `context_mark` closes a raw `S*` span and starts a labeled one without requiring a plan. `context_inspect` reports fixed prompt tokens separately from ordered raw and summary items, plus persisted selective-compaction jobs. `context_compact` accepts only ordered, adjacent, closed items, reserves them against overlap, queues background summarization, and immediately returns an independent `J*` job ID. A provider response may contain multiple separate context calls: the policy preserves their IDs, processes them in returned order across ordinary-tool barriers, and lets disjoint jobs run concurrently while invalid or overlapping calls produce independent errors. Jobs may finish in any order; the runtime applies successful summaries at safe agent-loop boundaries and never rewrites an in-flight provider request. Each resulting `C*` summary records token sizes and nested provenance. `context_wait` waits for explicit IDs or for the set of jobs pending when the call begins, with immediate terminal results and clear per-call unknown-ID errors. Job lifecycle states are `queued`, `running`, `applied`, `failed`, `cancelled`, and `stale`; non-successful terminal states leave active items unchanged and release their reservations. Session resume cancels jobs whose workers no longer exist, and full compaction supersedes pending selective jobs. The current open item and fixed instructions/tools are not selectable. When `context_compact` is available, pressure notices are advisory at 25%, encouraged at 50%, and high priority at 75%; only the highest newly crossed band is emitted, and selective or automatic compaction recomputes the next threshold from the reduced pressure. Automatic compaction still handles unmarked histories and preserves a recent tool-safe tail. Selective or forced compaction changes only the active projection; `$PHI_HOME/sessions/*/events.jsonl` remains the durable raw record and records job lifecycle events for replay.

Sessions use a flat durable home store. Workspace and temporary worktree paths are metadata and do not determine storage. Sessions preserve their original configuration, plugin sources, and plugin skill resources; changes apply to new or reloaded sessions, not already-open ones.
