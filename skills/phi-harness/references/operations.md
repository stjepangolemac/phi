# Operations

## Inspect

```sh
phi status
phi --json status
phi doctor
phi check-config
phi update-plugins
```

`status` reports the resolved composition and active `config.scm`. `doctor` checks installation health. `check-config` validates and smoke-replays the live configuration.

Phi checks official and installed plugin sources on startup. Run `/update-plugins` in the TUI or `phi update-plugins` in the shell when an update notice appears, then `/reload` to adopt updated plugin code and skills in the current conversation. Existing sessions continue reading their pinned skill resources until reloaded.

Run `/keys` in the TUI to see composer editing, history, scrolling, queueing, cancellation, picker, and approval controls. The same view exposes detailed input, cached-input, cache-write, and output token counters without expanding the normal status line. Ctrl+C cancels an active agent turn, quits when idle, and visibly reports when a running slash command cannot be cancelled.

Direct file reads and edits normally allow the workspace and Phi home. Writes still require approval. `phi --yolo` removes approval and filesystem boundaries.

## Runtime observability

Runtime logging is explicitly opt-in:

```sh
PHI_LOG=~/.phi/phi.jsonl phi
PHI_LOG=~/.phi/phi.jsonl PHI_RUNTIME_EVENTS=1 phi
```

`PHI_LOG` accepts an append-only file path or `-` for stdout. Each JSONL record carries a timestamp, event name, level, run correlation ID, and available session/task/call identifiers. High-value HTTP, process, policy, tool, session-write, workflow, cancellation, and failure boundaries are logged without request bodies, headers, secrets, encrypted reasoning, unrestricted arguments, process output, or conversation/model payloads. `PHI_RUNTIME_EVENTS=1` writes the separately sanitized frontend event sequence to the active session's `runtime.jsonl`.

Failure is explicit and storage-safe: sink initialization errors fail CLI startup; later sink or runtime-event tee write errors disable observability and emit one stderr diagnostic while normal session `events.jsonl` and `state.json` persistence continues. Avoid `PHI_LOG=-` when stdout is a JSON or RPC protocol channel.

## Reconfigure

Edit the `config.scm` path reported by `phi --json status`. Keep all plugin, provider, model, tool, prompt, compaction, and agent behavior configuration there; do not edit installed plugin files.

## Verify changes

```sh
phi check-config
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo install --path crates/phi-cli --force
```

Run `/reload`, or call `reload_config` from the agent, after changing `config.scm` or `config.json`. Reload validates the live composition before replacing the current session snapshot. If validation fails, the previous composition remains active. Skills are discovered for each turn.

Keep `README.md` and this skill current when changing configuration paths, CLI commands, extension contracts, or source precedence.
