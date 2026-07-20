import { spawn } from "node:child_process"
import { randomUUID } from "node:crypto"
import { createInterface } from "node:readline"

let runtime = null
let activePhase = ""
let totalAgents = 0
let runningAgents = 0
let available = 0
const waiters = []

export function configure(value) {
  runtime = value
  available = value.limits.maxConcurrency
}

function configured() {
  if (!runtime) throw new Error("phi:workflow is not configured")
  return runtime
}

async function acquire() {
  if (available > 0) {
    available -= 1
    return
  }
  await new Promise(resolve => waiters.push(resolve))
}

function release() {
  const next = waiters.shift()
  if (next) next()
  else available += 1
}

function progress(type, value = {}) {
  configured().progress({ type, phase: activePhase || null, ...value })
}

export function agent(prompt, options = {}) {
  const context = configured()
  const operation = runAgent(context, prompt, options)
  context.agentStarted?.(operation)
  operation.finally(() => context.agentFinished?.(operation)).catch(() => {})
  return operation
}

async function runAgent(context, prompt, options) {
  if (context.isClosing()) throw new Error("workflow is finishing")
  if (typeof prompt !== "string" || prompt.length === 0) {
    throw new TypeError("agent() requires a non-empty prompt")
  }
  const unknown = Object.keys(options).filter(key =>
    !["label", "schema", "branch", "branch_off"].includes(key)
  )
  if (unknown.length > 0) {
    throw new TypeError(`unsupported agent option: ${unknown[0]}`)
  }
  if (options.branch_off !== undefined && options.branch === undefined) {
    throw new TypeError("agent branch_off requires branch")
  }
  if (totalAgents >= context.limits.maxAgents) {
    throw new Error(`workflow agent limit exceeded (${context.limits.maxAgents})`)
  }
  totalAgents += 1
  const index = totalAgents
  const label = options.label ?? `agent-${index}`
  await acquire()
  runningAgents += 1
  let managed = null
  let managedBranch = null
  let childSessionId = null
  let relationship = null
  try {
    if (context.isClosing()) throw new Error("workflow is finishing")
    if (options.branch !== undefined) {
      managed = await context.worktrees.prepare(options.branch, options.branch_off)
      managedBranch = options.branch
    }
    if (context.isClosing()) throw new Error("workflow is finishing")
    const workspace = managed?.workspace ?? context.workspace
    const branchContext = managed?.promptContext ?? context.worktrees.promptContext()
    childSessionId = randomUUID()
    relationship = {
      childSessionId,
      parentSessionId: context.parentSessionId,
      workflowTaskId: context.taskId,
      agentLabel: label,
      logicalBranch: options.branch ?? null,
      branch: managed?.branch ?? null,
      workspace,
      worktreePath: managed?.worktreePath ?? null
    }
    await context.recordChild({ status: "allocating", ...relationship })
    await createChildSession(context, relationship)
    await context.recordChild({ status: "created", ...relationship })
    if (context.isClosing()) throw new Error("workflow is finishing")
    progress("agent_started", {
      index,
      label,
      branch: options.branch ?? null,
      workspace,
      childSessionId
    })
    const value = await runPhi(context, childSessionId, branchContext + prompt, options.schema, workspace, event => {
      if (event.type === "model_delta") {
        progress("agent_output", { index, label, content: event.content })
      }
    })
    await context.worktrees.finished(managedBranch, "completed")
    await context.recordChild({ status: "completed", ...relationship })
    progress("agent_completed", { index, label })
    return value
  } catch (error) {
    await context.worktrees.finished(managedBranch, "failed").catch(() => {})
    if (childSessionId && relationship) {
      const status = context.isClosing() ? "cancelled" : "failed"
      await context.recordChild({ status, error: error.message, ...relationship }).catch(() => {})
    }
    progress("agent_failed", { index, label, error: error.message })
    throw error
  } finally {
    runningAgents -= 1
    release()
  }
}

function createChildSession(context, relationship) {
  return runCommand(context, [
    "--workspace", relationship.workspace,
    "--yolo",
    "internal-create-session", relationship.childSessionId,
    "--parent-session", relationship.parentSessionId,
    "--workflow-task", relationship.workflowTaskId,
    "--agent-label", relationship.agentLabel,
    ...(relationship.branch ? ["--branch", relationship.branch] : []),
    ...(relationship.worktreePath ? ["--worktree-path", relationship.worktreePath] : [])
  ])
}

function runCommand(context, args) {
  const operation = new Promise((resolve, reject) => {
    const child = spawn(context.phi, args, { stdio: ["ignore", "pipe", "pipe"] })
    let stderr = ""
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", chunk => { stderr += chunk })
    child.on("error", reject)
    child.on("exit", code => {
      if (code === 0) resolve()
      else reject(new Error(stderr.trim() || `Phi exited with code ${code}`))
    })
  })
  context.setupStarted(operation)
  operation.finally(() => context.setupFinished(operation)).catch(() => {})
  return operation
}

function runPhi(context, childSessionId, prompt, schema, workspace, onEvent) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      context.phi,
      ["--workspace", workspace, "--yolo", "rpc", "--session", childSessionId],
      { stdio: ["pipe", "pipe", "pipe"] }
    )
    context.childStarted(child)
    let stderr = ""
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", chunk => { stderr += chunk })
    const lines = createInterface({ input: child.stdout })
    let settled = false
    lines.on("line", line => {
      let message
      try {
        message = JSON.parse(line)
      } catch {
        return
      }
      if (message.method === "agent.event") {
        onEvent(message.params)
      } else if (message.id === 1 && message.error) {
        settled = true
        reject(new Error(message.error.message ?? "Phi agent failed"))
      } else if (message.id === 1 && message.result) {
        settled = true
        resolve(message.result.value)
      }
    })
    child.on("error", error => {
      if (!settled) reject(error)
    })
    child.on("exit", code => {
      context.childFinished(child)
      if (!settled) {
        reject(new Error(stderr.trim() || `Phi agent exited with code ${code}`))
      }
    })
    child.stdin.end(JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "agent.run",
      params: { prompt, schema: schema ?? null }
    }) + "\n")
  })
}

function validateTasks(name, tasks) {
  if (!Array.isArray(tasks) || tasks.some(task => typeof task !== "function")) {
    throw new TypeError(`${name}() requires an array of functions`)
  }
}

function positiveInteger(name, value) {
  if (!Number.isInteger(value) || value < 1) {
    throw new RangeError(`${name} must be a positive integer`)
  }
  return value
}

function validateOptions(name, options, allowed) {
  if (options === null || typeof options !== "object" || Array.isArray(options)) {
    throw new TypeError(`${name}() options must be an object`)
  }
  const unknown = Object.keys(options).filter(key => !allowed.includes(key))
  if (unknown.length > 0) {
    throw new TypeError(`unsupported ${name} option: ${unknown[0]}`)
  }
}

export async function parallel(tasks, options = {}) {
  validateTasks("parallel", tasks)
  validateOptions("parallel", options, ["concurrency"])
  const concurrency = positiveInteger(
    "parallel concurrency",
    options.concurrency ?? Math.max(1, tasks.length)
  )
  if (tasks.length === 0) return []
  const results = new Array(tasks.length)
  let nextIndex = 0

  async function worker() {
    while (true) {
      const index = nextIndex
      nextIndex += 1
      if (index >= tasks.length) return
      results[index] = await Promise.resolve().then(tasks[index])
    }
  }

  await Promise.all(
    Array.from({ length: Math.min(concurrency, tasks.length) }, () => worker())
  )
  return results
}

export async function batch(tasks, options = {}) {
  validateTasks("batch", tasks)
  validateOptions("batch", options, ["size"])
  const size = positiveInteger("batch size", options.size)
  const results = []

  for (let start = 0; start < tasks.length; start += size) {
    const current = tasks.slice(start, start + size)
    results.push(...await parallel(current, { concurrency: size }))
  }
  return results
}

export async function pipeline(items, ...stages) {
  if (!Array.isArray(items) || stages.some(stage => typeof stage !== "function")) {
    throw new TypeError("pipeline() requires an array followed by stage functions")
  }
  return Promise.all(items.map(async (original, index) => {
    let value = original
    for (const stage of stages) value = await stage(value, original, index)
    return value
  }))
}

export function phase(title) {
  if (typeof title !== "string" || title.length === 0) {
    throw new TypeError("phase() requires a non-empty title")
  }
  activePhase = title
  progress("phase", { title })
}

export function log(message, data = null) {
  if (typeof message !== "string") throw new TypeError("log() requires a string")
  progress("log", { message, data })
}

export const budget = Object.freeze({
  get agentsUsed() { return totalAgents },
  get agentsRemaining() {
    return runtime ? Math.max(0, runtime.limits.maxAgents - totalAgents) : 0
  },
  get running() { return runningAgents },
  get concurrency() { return runtime?.limits.maxConcurrency ?? 0 }
})
