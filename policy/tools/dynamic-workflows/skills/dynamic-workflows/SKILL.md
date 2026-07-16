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

`agent(prompt, { label?, schema? })` starts a fresh `phi --workspace WORKSPACE --yolo rpc` child. A schema requests strict JSON-schema output. Use `Workflow` to launch, `TaskOutput` to inspect or wait, and `TaskStop` to cancel.
