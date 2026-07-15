# Operations

## Inspect

```sh
phi status
phi --json status
phi doctor
phi check-config
```

`status` reports the resolved composition and active `config.scm`. `doctor` checks installation health. `check-config` validates and smoke-replays the live configuration.

Direct file reads and edits normally allow the workspace and Phi home. Writes still require approval. `phi --yolo` removes approval and filesystem boundaries.

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
