---
name: phi-harness
description: Inspect, explain, configure, extend, or modify the Phi agent harness. Use for questions or work involving Phi configuration, policies, providers, models, plugins, tools, skills, prompt builders, compactors, file editors, sessions, installation, or self-modification.
---

# Phi harness

Run `phi --json status` before explaining or changing Phi. Treat its output, the active config file, and session snapshots as authoritative.

Read only the references needed for the task. Load them through `load_skill` using these paths; do not resolve them from the workspace:

- [architecture](references/architecture.md) for crate ownership and the request lifecycle.
- [configuration](references/configuration.md) for paths, state, and precedence.
- [extensions](references/extensions.md) for plugins and Steel extension points.
- [operations](references/operations.md) for inspection, validation, reload, installation, and troubleshooting.

Keep changes narrow. Inspect the active composition before editing, use existing commands and extension points, validate proportionally, then call `reload_config` to adopt the change in the current conversation.
