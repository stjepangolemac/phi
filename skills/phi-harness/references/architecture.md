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
7. Compact after a completed model/tool cycle when the selected model threshold is crossed, or immediately when the user runs `/compact`.

Sessions preserve their original configuration and plugin sources. Changes apply to new sessions, not already-open ones.
