import { readFile, writeFile, mkdir, rename, appendFile, copyFile } from "node:fs/promises"
import { randomUUID } from "node:crypto"
import { join } from "node:path"
import { fileURLToPath, pathToFileURL } from "node:url"
import { configure } from "./api.mjs"
import { createWorktreeManager } from "./worktrees.mjs"

const nodeProcess = globalThis.process
const requestPath = nodeProcess.argv[2]
if (!requestPath) throw new Error("workflow runner requires a request path")
const request = JSON.parse(await readFile(requestPath, "utf8"))
const taskDir = request.taskDir
const statePath = join(taskDir, "state.json")
const progressPath = join(taskDir, "progress.jsonl")
const summaryPath = join(taskDir, "summary.json")
const resultPath = join(taskDir, "result.json")
const children = new Set()
let progressQueue = Promise.resolve()
const summary = {
  phase: null,
  latestLog: null,
  agents: { started: 0, running: 0, completed: 0, failed: 0 }
}

async function atomicJson(path, value) {
  const temporary = `${path}.${nodeProcess.pid}.${randomUUID()}.tmp`
  await writeFile(temporary, JSON.stringify(value, null, 2) + "\n")
  await rename(temporary, path)
}

async function progress(event) {
  await appendFile(progressPath, JSON.stringify({ at: Date.now(), ...event }) + "\n")
  let changed = false
  if (typeof event.phase === "string" && event.phase.length > 0
      && summary.phase !== event.phase) {
    summary.phase = event.phase
    changed = true
  }
  if (event.type === "log") {
    summary.latestLog = event.message ?? null
    changed = true
  } else if (event.type === "agent_started") {
    summary.agents.started += 1
    summary.agents.running += 1
    changed = true
  } else if (event.type === "agent_completed") {
    summary.agents.running = Math.max(0, summary.agents.running - 1)
    summary.agents.completed += 1
    changed = true
  } else if (event.type === "agent_failed") {
    summary.agents.running = Math.max(0, summary.agents.running - 1)
    summary.agents.failed += 1
    changed = true
  }
  if (changed) {
    await atomicJson(summaryPath, { ...summary, updatedAt: Date.now() })
  }
}

function enqueueProgress(event) {
  progressQueue = progressQueue.then(() => progress(event))
  return progressQueue
}

async function stopChildren() {
  const running = [...children]
  for (const child of running) {
    try { child.kill("SIGTERM") } catch {}
  }
  await Promise.all(running.map(child => new Promise(resolve => {
    if (child.exitCode !== null) return resolve()
    const killTimer = setTimeout(() => {
      try { child.kill("SIGKILL") } catch {}
    }, 2_000)
    const fallbackTimer = setTimeout(resolve, 4_000)
    child.once("exit", () => {
      clearTimeout(killTimer)
      clearTimeout(fallbackTimer)
      resolve()
    })
  })))
}

let worktrees = null
let terminating = false
let closing = false
async function terminateFromSignal(code) {
  if (terminating) return
  terminating = true
  closing = true
  await stopChildren().catch(() => {})
  await worktrees?.cleanup().catch(() => {})
  nodeProcess.exit(code)
}

nodeProcess.on("SIGTERM", () => { void terminateFromSignal(143) })
nodeProcess.on("SIGINT", () => { void terminateFromSignal(130) })

function prepareSource(source, apiUrl) {
  if (/\bimport\s*\(/.test(source)) throw new Error("dynamic imports are not allowed")
  const imports = [...source.matchAll(/\bfrom\s*(["'])([^"']+)\1/g)]
  for (const match of imports) {
    if (match[2] !== "phi:workflow") {
      throw new Error(`workflow import is not allowed: ${match[2]}`)
    }
  }
  if (/\b(?:process|require|fetch|WebSocket|child_process)\b/.test(source)) {
    throw new Error("workflow uses a forbidden runtime API")
  }
  return source.replace(/(["'])phi:workflow\1/g, JSON.stringify(apiUrl))
}

await mkdir(taskDir, { recursive: true })
await atomicJson(statePath, {
  taskId: request.taskId,
  workflow: request.name,
  status: "running",
  startedAt: request.startedAt
})
await atomicJson(summaryPath, summary)

try {
  const sourcePath = request.workflowPath
  if (typeof sourcePath !== "string" || sourcePath.length === 0) {
    throw new Error("workflow runner requires a resolved workflow path")
  }
  const persistedPath = join(taskDir, "workflow.js")
  await copyFile(sourcePath, persistedPath)
  const source = await readFile(sourcePath, "utf8")
  const apiUrl = pathToFileURL(join(fileURLToPath(new URL(".", import.meta.url)), "api.mjs")).href
  const generatedPath = join(taskDir, "workflow.generated.mjs")
  await writeFile(generatedPath, prepareSource(source, apiUrl))
  worktrees = createWorktreeManager({
    taskId: request.taskId,
    workspace: request.workspace,
    git: request.git,
    worktreeRoot: request.worktreeRoot,
    persist: value => atomicJson(join(taskDir, "worktrees.json"), value),
    progress: (type, value) => enqueueProgress({ type, ...value })
  })
  configure({
    phi: request.phi,
    workspace: request.workspace,
    limits: request.limits,
    worktrees,
    isClosing: () => closing,
    progress: enqueueProgress,
    childStarted: child => children.add(child),
    childFinished: child => children.delete(child)
  })
  const module = await import(`${pathToFileURL(generatedPath).href}?task=${request.taskId}`)
  if (!module.meta || typeof module.meta.name !== "string"
      || typeof module.meta.description !== "string") {
    throw new Error("workflow must export meta with name and description")
  }
  if (typeof module.default !== "function") {
    throw new Error("workflow must default-export an async function")
  }
  await progress({ type: "workflow_started", name: request.name, meta: module.meta })
  let timer
  let value
  try {
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(
        () => reject(new Error(`workflow timed out after ${request.limits.timeoutMs} ms`)),
        request.limits.timeoutMs
      )
    })
    value = await Promise.race([module.default({ args: request.args }), timeout])
  } finally {
    clearTimeout(timer)
  }
  closing = true
  await stopChildren()
  await worktrees.cleanup()
  await progressQueue
  await atomicJson(resultPath, { value: value ?? null })
  await atomicJson(statePath, {
    taskId: request.taskId,
    workflow: request.name,
    status: "completed",
    startedAt: request.startedAt,
    completedAt: Date.now()
  })
} catch (error) {
  closing = true
  await stopChildren().catch(() => {})
  let cleanupError = null
  try {
    await worktrees?.cleanup()
  } catch (cleanup) {
    cleanupError = cleanup
  }
  await progressQueue.catch(() => {})
  await atomicJson(statePath, {
    taskId: request.taskId,
    workflow: request.name,
    status: "failed",
    error: [error?.stack ?? String(error), cleanupError?.stack ?? null]
      .filter(Boolean)
      .join("\n\nCleanup error:\n"),
    startedAt: request.startedAt,
    completedAt: Date.now()
  })
  nodeProcess.exitCode = 1
} finally {
  await stopChildren().catch(() => {})
}
