# Phi

Phi is a small, configurable agent harness. Rust owns trusted mechanisms such as HTTP, secrets, filesystem/process capabilities, sessions, and the terminal UI. Sandboxed [Steel](https://github.com/mattwparas/steel) code owns providers, prompts, compaction, commands, and the agent loop.

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
phi check-policy
phi run "Reply with exactly: phi works"
phi --json run "Stream this response"
phi --allow-shell run "List files in this directory"
phi resume SESSION_ID "Continue"
```

Sessions and their exact policy/plugin sources are stored under `.phi/sessions/` in the working directory.

## Configuration

Phi keeps behavior in Scheme and data in JSON:

```text
~/.phi/
  main.scm             # loads plugins and selects implementations
  config.json          # permissions, network origins, secret handles, limits
  state.json           # last selected model settings
  plugins.lock.json    # Git sources and exact commits
  plugins/             # immutable installed plugin revisions
  builtins/<version>/  # bundled fallback plugins and agent policy
```

`main.scm` is the composition root:

```scheme
(load-plugin! "openai")
(load-plugin! "simple-prompt")
(load-plugin! "simple-compaction")

(select-prompt-builder! "simple")
(select-compactor!
  "simple"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"))
```

There is no default provider. Providers register qualified model identities such as `openai/gpt-5.6-luna`; the selected model determines the provider. Use `/model` in the TUI to pick the model, reasoning, and provider-supported service tier.

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
```

Installation records the resolved commit but does not activate the plugin. Add `(load-plugin! "example")` to `~/.phi/main.scm` and compose its registered behavior there.

Plugins have no package-level type. Their entrypoints may register providers and models, prompt builders, compactors, or slash commands. The registration functions enforce the contract of each extension point.

## Workspace

```text
crates/phi-cli       installed `phi` binary
crates/phi-core      trusted capabilities, sessions, plugins, and home layout
crates/phi-eval      candidate policy validation
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
