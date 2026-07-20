# Configuration

Run `phi --json status` first to see the resolved composition and active configuration path.

## Phi home

`PHI_HOME` selects the home directory; otherwise Phi uses `~/.phi`.

`PHI_LOG=<path|->` opts into append-only structured JSONL runtime logging. A path sink may create missing parent directories; `-` writes to stdout. `PHI_RUNTIME_EVENTS=1` additionally tees sanitized runtime events to the active session's `runtime.jsonl`. Both features are disabled when the variables are absent.

- `config.scm`: the sole mutable Scheme configuration root. It defines agent behavior, loads plugins, configures providers and tools, and selects implementations.
- `config.json`: allowed network origins and secret handles.
- `state.json`: last selected model, reasoning, and service tier.
- `plugins.lock.json`: Git sources and resolved commits.
- `plugins/`: immutable installed plugin packages.
- `skills/`: manually copied personal skills.
- `sessions/<session-id>/`: durable flat conversation storage, including exact composition snapshots, `events.jsonl`, `state.json`, workflow runs, numbered plans, and opt-in sanitized `runtime.jsonl` observability.
- `builtins/<version>/`: the official plugin snapshot embedded in that Phi build, plus bundled system skills.

Never put secret values in `config.scm`, plugin configuration, status output, or the repository. Secret handles in `config.json` point to separate files.

Do not edit installed or official plugin files to reconfigure Phi. All provider, model, tool, prompt, compaction, and agent behavior changes belong in `config.scm`. If a plugin does not expose a required setting, extend its configuration interface rather than patching an installed package.

## Tool approval policy

`config.scm` may call `set-tool-approval-policy!` with a two-argument handler receiving the tool name and parsed JSON arguments. The handler returns `(hash 'decision "allow|ask|deny" 'detail "concise invocation")`. Rust remains the trusted enforcement point: it combines the CLI fallback with the hook and applies the stricter decision, so Steel can attenuate `--allow-shell` and `--allow-write` but cannot grant capabilities those flags withhold. If no hook is configured, the existing CLI flags are the fallback; `--yolo` explicitly bypasses approval policy. Invalid output and hook errors fail closed. Non-interactive runs cannot satisfy `ask`, so both `ask` and `deny` return tool errors without execution.

The bundled hook renders approval detail and permits simple read-only Git commands while asking for local mutation, remote mutation, shell metacharacters, and ambiguous Git syntax. Rust mirrors that Git minimum so older configurations without the bundled hook receive the same intentional hardening.

## Workspace state

- `.phi/skills/`: workspace skills; these override personal skills with the same name.
- `.phi/workflows/`: workspace-specific reusable workflow definitions.

Workspace and worktree paths are session metadata only. New conversations and plans are never stored under workspace `.phi/`. Existing workspace-local `.phi/sessions/` and `.phi/PLAN.md` data is left untouched and is not migrated or resumed implicitly.

Every installed plugin may expose `skills/NAME/SKILL.md` without being loaded. Skill precedence is protected system skills, workspace skills, personal skills, then plugin skills. Duplicate names from different installed plugins are configuration errors.

## Reload behavior

New conversations load the current `config.scm` and snapshot installed plugin skills. Existing conversations keep their snapshot until `/reload` or `reload_config` validates and adopts the live configuration and skill set. Failed validation leaves the previous composition active.
