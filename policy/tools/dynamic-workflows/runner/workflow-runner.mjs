import { readFile, writeFile, mkdir, rename, appendFile, copyFile } from "node:fs/promises"
import { join } from "node:path"
import { fileURLToPath, pathToFileURL } from "node:url"
import { configure } from "./api.mjs"

const nodeProcess = globalThis.process
const requestPath = nodeProcess.argv[2]
if (!requestPath) throw new Error("workflow runner requires a request path")
const request = JSON.parse(await readFile(requestPath, "utf8"))
const taskDir = request.taskDir
const statePath = join(taskDir, "state.json")
const progressPath = join(taskDir, "progress.jsonl")
const resultPath = join(taskDir, "result.json")
const children = new Set()
let progressQueue = Promise.resolve()

async function atomicJson(path, value) {
  const temporary = `${path}.tmp`
  await writeFile(temporary, JSON.stringify(value, null, 2) + "\n")
  await rename(temporary, path)
}

async function progress(event) {
  await appendFile(progressPath, JSON.stringify({ at: Date.now(), ...event }) + "\n")
}

function enqueueProgress(event) {
  progressQueue = progressQueue.then(() => progress(event))
  return progressQueue
}

function stopChildren() {
  for (const child of children) {
    try { child.kill("SIGTERM") } catch {}
  }
}

nodeProcess.on("SIGTERM", () => { stopChildren(); nodeProcess.exit(143) })
nodeProcess.on("SIGINT", () => { stopChildren(); nodeProcess.exit(130) })

function safeName(name) {
  return typeof name === "string"
    && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name)
    && name !== "." && name !== ".."
}

async function readable(path) {
  try { await readFile(path); return true } catch { return false }
}

async function resolveWorkflow(name) {
  if (!safeName(name)) throw new Error(`invalid workflow name: ${name}`)
  const candidates = [
    join(request.workspace, ".phi", "workflows", `${name}.js`),
    join(request.home, "workflows", `${name}.js`),
    ...request.pluginDirs.map(root => join(root, "workflows", `${name}.js`))
  ]
  for (const candidate of candidates) {
    if (await readable(candidate)) return candidate
  }
  throw new Error(`workflow not found: ${name}`)
}

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

try {
  const sourcePath = await resolveWorkflow(request.name)
  const persistedPath = join(taskDir, "workflow.js")
  await copyFile(sourcePath, persistedPath)
  const source = await readFile(sourcePath, "utf8")
  const apiUrl = pathToFileURL(join(fileURLToPath(new URL(".", import.meta.url)), "api.mjs")).href
  const generatedPath = join(taskDir, "workflow.generated.mjs")
  await writeFile(generatedPath, prepareSource(source, apiUrl))
  configure({
    phi: request.phi,
    workspace: request.workspace,
    limits: request.limits,
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
  await progressQueue.catch(() => {})
  await atomicJson(statePath, {
    taskId: request.taskId,
    workflow: request.name,
    status: "failed",
    error: error?.stack ?? String(error),
    completedAt: Date.now()
  })
  nodeProcess.exitCode = 1
} finally {
  stopChildren()
}
