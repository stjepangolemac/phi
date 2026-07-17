---
name: dynamic-workflows
description: Create, run, inspect, and troubleshoot Phi JavaScript workflows and child-agent orchestration.
---

# Dynamic workflows

Use named JavaScript workflows for multi-agent orchestration. Workflows are discovered from `.phi/workflows/NAME.js`, `~/.phi/workflows/NAME.js`, then `workflows/NAME.js` in loaded plugins.

Modules export `meta` with `name` and `description`, plus a default async `({ args }) => value` function. Import `agent`, `parallel`, `batch`, `pipeline`, `phase`, `log`, and `budget` from `phi:workflow`.

```js
import { agent, parallel, phase } from "phi:workflow"

export const meta = {
  name: "review",
  description: "Review a change from several perspectives."
}

export default async function ({ args }) {
  phase("Review")
  return parallel(args.files.map((file, index) => () =>
    agent(`Review file ${index + 1}:\n${file}`, { label: `review-${index + 1}` })
  ), { concurrency: 3 })
}
```

Tasks passed to `parallel` or `batch` must be functions so work starts only when scheduled. `parallel` continuously fills its concurrency limit; `batch` runs fixed-size waves. Workflows are limited to 8 concurrent agents, 32 total agents, and 60 minutes.

`agent(prompt, { label?, schema?, branch?, branch_off? })` starts a fresh `phi --workspace WORKSPACE --yolo rpc` child. A schema requests strict JSON-schema output. Use `Workflow` to launch, `TaskOutput` to inspect or wait, and `TaskStop` to cancel.

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

Branched agents run in the equivalent repository-relative subdirectory when Phi was launched below the repository root. They should commit their work and return commit hashes through schemas. Worktrees and their task-owned refs are temporary and are always removed on workflow exit, including cancellation and failure. Before the workflow exits, promote every wanted commit to a non-managed ref with an explicit unbranched `agent()` call. Never rely on a managed ref surviving the workflow.
