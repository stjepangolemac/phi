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
phi check-policy
phi run "Reply with exactly: phi works"
phi --json run "Stream this response"
phi --allow-shell run "List files in this directory"
phi --yolo # run all tools without approval
phi resume SESSION_ID "Continue"
```

Sessions and their exact policy/plugin sources are stored under `.phi/sessions/` in the working directory.

The shell tools run arbitrary commands through the user's shell, including pipelines and compound commands. Long-running commands yield a background session that survives model turns; the model can list sessions, poll them, or continue through stdin. Use `/ps` to inspect background processes and `/stop` to stop them. PTYs are available for interactive programs. Tool approval is still required unless `--allow-shell` or `--yolo` is used. OS sandboxing is intentionally not implemented yet.

## Configuration

Phi keeps behavior in Scheme and data in JSON:

```text
~/.phi/
  main.scm             # loads plugins and selects implementations
  config.json          # permissions, network origins, and secret handles
  state.json           # last selected model settings
  plugins.lock.json    # Git sources and exact commits
  plugins/             # immutable installed plugin revisions
  skills/              # manually copied personal skills
  builtins/<version>/  # bundled fallback plugins and agent policy
```

`main.scm` is the composition root:

```scheme
(load-plugin! "responses")
(load-plugin! "openai")
(load-plugin! "openrouter")
(load-plugin! "openai-web-search")
(load-plugin! "openrouter-web-search")
(load-plugin! "skills")
(load-plugin! "codex-patch")
(load-plugin! "simple-prompt")
(load-plugin! "simple-compaction")

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
  "simple"
  (hash 'model "openai/gpt-5.6-luna"
        'reasoning "low"
        'service_tier "default"))
```

The selected file editor owns its model-facing format and matching logic in Steel. Rust supplies contained file snapshots, checks revisions, requests write approval, and persists the proposed changes. The bundled `codex-patch` editor exposes one `patch` tool for add, update, delete, and move operations.

There is no default provider. Providers register qualified model identities such as `openai/gpt-5.6-luna`; the selected model determines the provider. Use `/model` in the TUI to pick the model, reasoning, and provider-supported service tier.

Tool implementations declare model compatibility. The configuration above prefers search hosted by the selected model's provider route, then explicitly falls back to a separate OpenAI search request. Put an OpenRouter key in `~/.phi/secrets/openrouter.json` as `{"api_key":"..."}` before selecting an `openrouter/...` model.

## Skills

Copy standard `SKILL.md` directories into `~/.phi/skills/` for personal use or `.phi/skills/` for one workspace:

```text
~/.phi/skills/review/
  SKILL.md
  references/
```

Workspace skills override personal skills with the same frontmatter name. Phi initially exposes only names and descriptions; the bundled `skills` plugin loads `SKILL.md` or a referenced file when needed. Use `/skills` to list discovered skills or mention `$skill-name` to request one explicitly.

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

## Optional features

These are possible extensions, not committed scope. Do not implement them without explicit operator approval:

- file review
- layered project instructions
- sandbox and permission modes
- turn steering and structured questions
- richer session management
- MCP integration
- plans and task tracking
- checkpoints and rewind
- lifecycle hooks
- subagents and worktrees
- durable memory
- multimodal, editor, and browser context

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
