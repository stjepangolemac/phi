import { spawn } from "node:child_process"
import { randomUUID } from "node:crypto"
import { createInterface } from "node:readline"

let runtime = null
let activePhase = ""
let totalAgents = 0
let runningAgents = 0
let available = 0
const waiters = []
const cancellationGraceMs = 2_500

class AgentTimeoutError extends Error {
  constructor(timeoutMs) {
    super(`agent timed out after ${timeoutMs} ms`)
    this.name = "AgentTimeoutError"
  }
}

export function configure(value) {
  runtime = value
  available = value.limits.maxConcurrency
}

function configured() {
  if (!runtime) throw new Error("phi:workflow is not configured")
  return runtime
}

async function acquire(signal) {
  if (signal.aborted) throw signal.reason
  if (available > 0) {
    available -= 1
    return
  }
  await new Promise((resolve, reject) => {
    const waiter = { resolve, reject, signal, abort: null }
    waiter.abort = () => {
      const index = waiters.indexOf(waiter)
      if (index !== -1) waiters.splice(index, 1)
      reject(signal.reason)
    }
    signal.addEventListener("abort", waiter.abort, { once: true })
    waiters.push(waiter)
  })
}

function release() {
  const next = waiters.shift()
  if (next) {
    next.signal.removeEventListener("abort", next.abort)
    next.resolve()
  }
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
    ![
      "label", "schema", "branch", "branch_off", "model", "reasoning",
      "timeout_ms", "capabilities"
    ].includes(key)
  )
  if (unknown.length > 0) {
    throw new TypeError(`unsupported agent option: ${unknown[0]}`)
  }
  if (options.branch_off !== undefined && options.branch === undefined) {
    throw new TypeError("agent branch_off requires branch")
  }
  const execution = resolveExecution(context, options)
  if (totalAgents >= context.limits.maxAgents) {
    throw new Error(`workflow agent limit exceeded (${context.limits.maxAgents})`)
  }
  totalAgents += 1
  const index = totalAgents
  const label = options.label ?? `agent-${index}`
  const childSessionId = randomUUID()
  let relationship = {
    childSessionId,
    parentSessionId: context.parentSessionId,
    workflowTaskId: context.taskId,
    agentLabel: label,
    logicalBranch: options.branch ?? null,
    branch: null,
    workspace: context.workspace,
    worktreePath: null,
    model: execution.model,
    reasoning: execution.reasoning,
    serviceTier: execution.serviceTier,
    timeoutMs: execution.timeoutMs,
    capabilityProfile: execution.capabilityProfile
  }
  const controller = new AbortController()
  const timeout = execution.expired ? null : setTimeout(
    () => controller.abort(new AgentTimeoutError(execution.timeoutMs)), execution.timeoutMs
  )
  if (execution.expired) controller.abort(new AgentTimeoutError(0))
  let acquired = false
  let managed = null
  let managedBranch = null
  let childCreated = false
  try {
    await acquire(controller.signal)
    acquired = true
    runningAgents += 1
    if (context.isClosing()) throw new Error("workflow is finishing")
    if (options.branch !== undefined) {
      managed = await context.worktrees.prepare(
        options.branch, options.branch_off, controller.signal
      )
      managedBranch = options.branch
    }
    if (controller.signal.aborted) throw controller.signal.reason
    if (context.isClosing()) throw new Error("workflow is finishing")
    const workspace = managed?.workspace ?? context.workspace
    const branchContext = managed?.promptContext ?? context.worktrees.promptContext()
    relationship = {
      ...relationship,
      branch: managed?.branch ?? null,
      workspace,
      worktreePath: managed?.worktreePath ?? null,
    }
    await context.recordChild({ status: "allocating", ...relationship })
    await createChildSession(context, relationship, execution, controller.signal)
    childCreated = true
    await context.recordChild({ status: "created", ...relationship })
    if (context.isClosing()) throw new Error("workflow is finishing")
    progress("agent_started", {
      index,
      label,
      branch: options.branch ?? null,
      workspace,
      childSessionId
    })
    const value = await runPhi(context, childSessionId, branchContext + prompt, options.schema, workspace, execution, controller.signal, event => {
      if (event.type === "model_delta") {
        progress("agent_output", { index, label, content: event.content })
      }
    })
    await context.worktrees.finished(managedBranch, "completed")
    await context.recordChild({ status: "completed", ...relationship })
    progress("agent_completed", { index, label })
    return value
  } catch (error) {
    const timedOut = error instanceof AgentTimeoutError || controller.signal.reason instanceof AgentTimeoutError
    const status = timedOut ? "timed_out" : context.isClosing() ? "cancelled" : "failed"
    await context.worktrees.finished(managedBranch, status).catch(() => {})
    await context.recordChild({ status, launched: childCreated, error: error.message, ...relationship }).catch(() => {})
    progress("agent_failed", { index, label, status, error: error.message })
    throw error
  } finally {
    clearTimeout(timeout)
    if (acquired) {
      runningAgents -= 1
      release()
    }
  }
}

function resolveExecution(context, options) {
  const parent = context.agentContext
  const model = options.model ?? parent.model
  if (typeof model !== "string" || model.length === 0) {
    throw new TypeError("agent model must be a non-empty string")
  }
  const spec = parent.models.find(candidate => candidate.id === model)
  if (!spec) throw new Error(`unknown agent model: ${model}`)
  const reasoning = options.reasoning ?? parent.reasoning
  if (typeof reasoning !== "string") throw new TypeError("agent reasoning must be a string")
  if (spec.reasoning.length > 0 && !spec.reasoning.some(option =>
    (typeof option === "string" ? option : option.id) === reasoning
  )) {
    throw new Error(`unsupported agent reasoning for ${model}: ${reasoning}`)
  }
  const serviceTier = options.model === undefined
    ? parent.serviceTier
    : spec.default_service_tier
  const requestedTimeout = options.timeout_ms ?? context.limits.maxDurationMs
  if (!Number.isInteger(requestedTimeout) || requestedTimeout < 1
      || requestedTimeout > context.limits.maxDurationMs) {
    throw new RangeError(
      `agent timeout_ms must be an integer between 1 and ${context.limits.maxDurationMs}`
    )
  }
  const remaining = context.deadlineAt - Date.now()
  const timeoutMs = Math.min(requestedTimeout, Math.max(1, remaining - cancellationGraceMs))
  const capabilityProfile = options.capabilities ?? "parent"
  if (!["parent", "read-only", "workspace-write"].includes(capabilityProfile)) {
    throw new TypeError(`unsupported agent capabilities profile: ${capabilityProfile}`)
  }
  if (capabilityProfile === "workspace-write"
      && !parent.allowWrite && !parent.fullAccess) {
    throw new Error("agent workspace-write capabilities exceed parent authority")
  }
  const capabilities = capabilityProfile === "parent"
    ? {
        allowShell: parent.allowShell,
        allowWrite: parent.allowWrite,
        fullAccess: parent.fullAccess,
        workspaceOnly: parent.workspaceOnly === true
      }
    : capabilityProfile === "workspace-write"
      ? { allowShell: false, allowWrite: true, fullAccess: false, workspaceOnly: true }
      : { allowShell: false, allowWrite: false, fullAccess: false, workspaceOnly: true }
  return {
    model, reasoning, serviceTier, timeoutMs, capabilityProfile, capabilities,
    expired: remaining <= 0
  }
}

function permissionArgs(execution) {
  if (execution.capabilities.fullAccess) return ["--yolo"]
  return [
    ...(execution.capabilities.allowShell ? ["--allow-shell"] : []),
    ...(execution.capabilities.allowWrite ? ["--allow-write"] : []),
    ...(execution.capabilities.workspaceOnly ? ["--workspace-only"] : [])
  ]
}

function createChildSession(context, relationship, execution, signal) {
  return runCommand(context, [
    "--workspace", relationship.workspace,
    ...permissionArgs(execution),
    "internal-create-session", relationship.childSessionId,
    "--parent-session", relationship.parentSessionId,
    "--workflow-task", relationship.workflowTaskId,
    "--agent-label", relationship.agentLabel,
    ...(relationship.branch ? ["--branch", relationship.branch] : []),
    ...(relationship.worktreePath ? ["--worktree-path", relationship.worktreePath] : []),
    "--model", execution.model,
    "--reasoning", execution.reasoning,
    "--service-tier", execution.serviceTier,
    "--timeout-ms", String(execution.timeoutMs),
    "--capability-profile", execution.capabilityProfile
  ], signal)
}

function runCommand(context, args, signal) {
  if (signal.aborted) return Promise.reject(signal.reason)
  const operation = new Promise((resolve, reject) => {
    const child = spawn(context.phi, args, { stdio: ["ignore", "pipe", "pipe"] })
    context.childStarted(child)
    let stderr = ""
    let settled = false
    let abortReason = null
    let killTimer = null
    const abort = () => {
      if (settled) return
      abortReason = signal.reason
      try { child.kill("SIGTERM") } catch {}
      killTimer = setTimeout(() => {
        try { child.kill("SIGKILL") } catch {}
      }, 2_000)
    }
    signal.addEventListener("abort", abort, { once: true })
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", chunk => { stderr += chunk })
    child.on("error", error => {
      if (!settled) {
        settled = true
        reject(abortReason ?? error)
      }
    })
    child.on("exit", code => {
      context.childFinished(child)
      signal.removeEventListener("abort", abort)
      clearTimeout(killTimer)
      if (settled) return
      settled = true
      if (abortReason) reject(abortReason)
      else if (code === 0) resolve()
      else reject(new Error(stderr.trim() || `Phi exited with code ${code}`))
    })
  })
  context.setupStarted(operation)
  operation.finally(() => context.setupFinished(operation)).catch(() => {})
  return operation
}

function runPhi(context, childSessionId, prompt, schema, workspace, execution, signal, onEvent) {
  if (signal.aborted) return Promise.reject(signal.reason)
  return new Promise((resolve, reject) => {
    const child = spawn(
      context.phi,
      ["--workspace", workspace, ...permissionArgs(execution), "rpc", "--session", childSessionId],
      { stdio: ["pipe", "pipe", "pipe"] }
    )
    context.childStarted(child)
    let stderr = ""
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", chunk => { stderr += chunk })
    const lines = createInterface({ input: child.stdout })
    let settled = false
    let abortReason = null
    let killTimer = null
    const abort = () => {
      if (settled) return
      abortReason = signal.reason
      try { child.kill("SIGTERM") } catch {}
      killTimer = setTimeout(() => {
        try { child.kill("SIGKILL") } catch {}
      }, 2_000)
    }
    signal.addEventListener("abort", abort, { once: true })
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
      if (!settled) {
        settled = true
        reject(abortReason ?? error)
      }
    })
    child.on("exit", code => {
      context.childFinished(child)
      signal.removeEventListener("abort", abort)
      clearTimeout(killTimer)
      if (!settled) {
        settled = true
        if (abortReason) reject(abortReason)
        else reject(new Error(stderr.trim() || `Phi agent exited with code ${code}`))
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
