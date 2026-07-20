# Configuration

Run `phi --json status` first to see the resolved composition and active configuration path.

## Phi home

`PHI_HOME` selects the home directory; otherwise Phi uses `~/.phi`.

- `config.scm`: the sole mutable Scheme configuration root. It defines agent behavior, loads plugins, configures providers and tools, and selects implementations.
- `config.json`: allowed network origins and secret handles.
- `state.json`: last selected model, reasoning, and service tier.
- `plugins.lock.json`: Git sources and resolved commits.
- `plugins/`: immutable installed plugin packages.
- `skills/`: manually copied personal skills.
- `sessions/<session-id>/`: durable flat conversation storage, including exact composition snapshots, `events.jsonl`, `state.json`, workflow runs, and numbered plans.
- `builtins/<version>/`: the official plugin snapshot embedded in that Phi build, plus bundled system skills.

Never put secret values in `config.scm`, plugin configuration, status output, or the repository. Secret handles in `config.json` point to separate files.

Do not edit installed or official plugin files to reconfigure Phi. All provider, model, tool, prompt, compaction, and agent behavior changes belong in `config.scm`. If a plugin does not expose a required setting, extend its configuration interface rather than patching an installed package.

## Workspace state

- `.phi/skills/`: workspace skills; these override personal skills with the same name.
- `.phi/workflows/`: workspace-specific reusable workflow definitions.

Workspace and worktree paths are session metadata only. New conversations and plans are never stored under workspace `.phi/`. Existing workspace-local `.phi/sessions/` and `.phi/PLAN.md` data is left untouched and is not migrated or resumed implicitly.

Every installed plugin may expose `skills/NAME/SKILL.md` without being loaded. Skill precedence is protected system skills, workspace skills, personal skills, then plugin skills. Duplicate names from different installed plugins are configuration errors.

## Reload behavior

New conversations load the current `config.scm` and snapshot installed plugin skills. Existing conversations keep their snapshot until `/reload` or `reload_config` validates and adopts the live configuration and skill set. Failed validation leaves the previous composition active.
