# Architecture

Phi is a Rust-hosted agent harness whose behavior is composed in sandboxed Steel.

## Ownership

- `phi-cli`: installed `phi` binary and non-TUI commands.
- `phi-core`: trusted HTTP, secrets, filesystem, processes, sessions, plugins, skills, and policy storage.
- `phi-protocol`: provider-neutral events, effects, tools, commands, and model metadata.
- `phi-runtime`: frontend-neutral agent loop and active composition.
- `phi-steel`: Steel VM, registries, validation, and policy execution.
- `phi-tui`: Ratatui frontend and transcript rendering.
- `policy/`: bundled agent, providers, tools, prompts, and compaction implementations.

Rust owns mechanisms that require containment or reliability. Steel owns provider behavior, prompt assembly, tool routing, compaction policy, editor formats, commands, and the agent loop.

## Turn lifecycle

1. Resolve `config.scm` and the plugin packages it loads.
2. Snapshot that composition into a new workspace session.
3. Build a provider-neutral prompt from instructions, messages, and compatible tools.
4. Let the selected provider plugin build and stream the request.
5. Execute returned tool calls through Rust capabilities or configured callable tools.
6. Feed results back until the policy finishes the turn.
7. Compact after a completed model/tool cycle when the selected model threshold is crossed, immediately when the user runs `/compact`, or selectively when the model compacts closed context items.

## Active context

The active model-facing context is an ordered reduction over immutable session history. `context_mark` closes a raw `S*` span and starts a labeled one without requiring a plan. `context_inspect` reports fixed prompt tokens separately from ordered raw and summary items. `context_compact` accepts only ordered, adjacent, closed items and creates one `C*` summary with token sizes and nested provenance. The current open item and fixed instructions/tools are not selectable. Providers dispatch context tools serially so each exposed call reaches the stateful policy handler for its turn. When `context_compact` is available, pressure notices are advisory at 25%, encouraged at 50%, and high priority at 75%; only the highest newly crossed band is emitted, and selective or automatic compaction recomputes the next threshold from the reduced pressure. Automatic compaction still handles unmarked histories and preserves a recent tool-safe tail. Selective or forced compaction changes only the active projection; `.phi/sessions/*/events.jsonl` remains the durable raw record.

Sessions preserve their original configuration and plugin sources. Changes apply to new sessions, not already-open ones.
