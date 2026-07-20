---
name: dynamic-workflows
description: Create, run, inspect, and troubleshoot Phi JavaScript workflows and child-agent orchestration.
---

# Dynamic workflows

Use named JavaScript workflows for multi-agent orchestration. Keep workflow definitions separate from their runs:

- `~/.phi/workflows/` contains global reusable definitions.
- `.phi/workflows/` contains workspace-specific definitions.
- Loaded plugins may package definitions under `workflows/`.
- `$PHI_HOME/sessions/<parent-session-id>/workflows/<task-id>/` contains durable run state, copied source, progress, logs, results, and child relationship records.

When looking for an existing definition, inspect global workflows first, then workspace and loaded-plugin workflows. Name-only `Workflow` calls use that same global → workspace → plugin precedence. Create workspace-specific definitions in `.phi/workflows/` normally. Only when the user explicitly asks to make one global, copy or promote it into `~/.phi/workflows/`.

Pass optional `path` to select an exact same-named definition instead of using discovery precedence. Relative paths resolve from the current workspace, while absolute paths are accepted within the same allowed roots. The path must be a regular `.js` file inside the global, current-workspace, or a loaded-plugin workflow root; traversal, escaping symlinks, unloaded-plugin paths, and outside paths are rejected. The module's `meta.name` must match the requested `name`. Each launch still gets a unique session-local task ID and run directory.

Modules export `meta` with `name` and `description`, plus a default async `({ args }) => value` function. Import `agent`, `parallel`, `batch`, `pipeline`, `phase`, `log`, and `budget` from `phi:workflow`.

```js
import { agent, parallel, phase } from "phi:workflow"

export const meta = {
  name: "review",
  description: "Review a change from several perspectives.",
  inputSchema: {
    type: "object",
    properties: { files: { type: "array", items: { type: "string" } } },
    required: ["files"],
    additionalProperties: false
  }
}

export default async function ({ args }) {
  phase("Review")
  return parallel(args.files.map((file, index) => () =>
    agent(`Review file ${index + 1}:\n${file}`, { label: `review-${index + 1}` })
  ), { concurrency: 3 })
}
```

Declare optional `meta.inputSchema` when callers need discoverable, validated inputs. Phi adds name-discovered schemas to the `Workflow` tool description and returns `description` plus `input_schema` from successful name or exact-path launches. It validates the schema and `Workflow.args` before task, runner, or child creation, with JSON Pointer-style instance and schema paths in errors. Omitting the schema preserves arbitrary JSON args.

The supported draft 2020-12/draft-07 subset includes boolean schemas, types, enums/constants, object properties/required/additional properties, homogeneous array items, size and numeric limits, patterns, uniqueness, and schema combinators. Standard annotations are accepted. Unsupported keywords such as `$ref`, `$defs`, conditionals, tuple/prefix items, dependencies, and formats fail explicitly at their schema path.

Tasks passed to `parallel` or `batch` must be functions so work starts only when scheduled. `parallel` continuously fills its concurrency limit; `batch` runs fixed-size waves. Workflows are limited to 8 concurrent agents, 32 total agents, and 60 minutes.

`agent(prompt, { label?, schema?, branch?, branch_off? })` allocates a fresh durable child session under `$PHI_HOME/sessions/` before starting `phi --workspace WORKSPACE --yolo rpc --session CHILD_ID`. The run records parent, task, label, branch, workspace, and worktree relationships. A schema requests strict JSON-schema output. Use `Workflow` with `name`, optional exact `path`, and `args` to launch; use `TaskOutput` to inspect or wait and `TaskStop` to cancel.

For one focused child, use the bundled `delegate` workflow. Pass the prompt and the same currently supported `agent()` options under `options`; omit `options` for a plain text result:

```json
{
  "name": "delegate",
  "args": {
    "prompt": "Summarize the current diff.",
    "options": {
      "label": "summary",
      "schema": {
        "type": "object",
        "properties": { "summary": { "type": "string" } },
        "required": ["summary"],
        "additionalProperties": false
      }
    }
  }
}
```

The launch is an ordinary durable workflow task: inspect it with `TaskOutput`, stop it with `TaskStop`, and expect the same child/task terminal records for success, failure, or cancellation as any other workflow.

## Managed Git branches

Set `branch` to a workflow-local logical name when an agent should edit in an isolated Git worktree:

```js
const feature = await agent("Implement and commit the feature. Return the commit hash.", {
  label: "feature",
  branch: "feature",
  schema: {
    type: "object",
    properties: { commit: { type: "string" } },
    required: ["commit"],
    additionalProperties: false
  }
})

await agent("Merge the completed feature branch, test it, and commit the integration.", {
  branch: "integration",
  branch_off: "feature"
})

await agent("Promote the completed integration branch into the original checkout and verify it.")
```

Without `branch_off`, Phi branches from the immutable commit captured when the workflow launched. `branch_off` is valid only with `branch`; it may identify an external Git ref or a completed managed logical branch. Managed logical names are mapped to task-owned `phi/...` refs, and agents receive the mapping needed to merge completed managed branches.

Branched agents run in the equivalent repository-relative subdirectory when Phi was launched below the repository root. They should commit their work and return commit hashes through schemas. Worktrees and their task-owned refs are temporary and are always removed on workflow exit, including cancellation and failure, while child sessions and run records remain durable. Before the workflow exits, promote every wanted commit to a non-managed ref with an explicit unbranched `agent()` call. Never rely on a managed ref surviving the workflow.
