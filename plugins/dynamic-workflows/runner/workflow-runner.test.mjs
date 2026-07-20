import assert from "node:assert/strict"
import { execFileSync, spawn } from "node:child_process"
import { chmod, mkdtemp, mkdir, readFile, realpath, rm, stat, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import test from "node:test"
import { fileURLToPath } from "node:url"

const runner = new URL("./workflow-runner.mjs", import.meta.url)
const runnerPath = fileURLToPath(runner)

function git(root, ...args) {
  return execFileSync("git", ["-C", root, ...args], { encoding: "utf8" }).trim()
}

async function fixture(source, args = null, name = "managed-test", overrides = {}) {
  const base = await mkdtemp(join(tmpdir(), "phi-workflow-runner-test-"))
  const repository = join(base, "repository")
  const home = join(base, "home")
  const taskDir = join(base, "task")
  const taskId = "66666666-6666-4666-8666-666666666666"
  await mkdir(repository)
  await mkdir(home)
  await mkdir(taskDir)
  git(repository, "init", "-q")
  git(repository, "config", "user.name", "Phi Test")
  git(repository, "config", "user.email", "phi@example.invalid")
  await writeFile(join(repository, "base.txt"), "base\n")
  git(repository, "add", ".")
  git(repository, "commit", "-qm", "base")
  const startingCommit = git(repository, "rev-parse", "HEAD")
  const workflowPath = join(base, "workflow.js")
  await writeFile(workflowPath, source)
  const fakePhi = join(base, "fake-phi.mjs")
  await writeFile(fakePhi, `#!/usr/bin/env node
import { execFileSync } from "node:child_process"
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs"
const home = ${JSON.stringify(join(base, "home"))}
appendFileSync(${JSON.stringify(join(base, "invocations.jsonl"))}, JSON.stringify(process.argv.slice(2)) + "\\n")
const createIndex = process.argv.indexOf("internal-create-session")
if (createIndex !== -1) {
  const label = process.argv[process.argv.indexOf("--agent-label") + 1]
  if (label === "setup-hang") await new Promise(() => {})
  const id = process.argv[createIndex + 1]
  const session = home + "/sessions/" + id
  mkdirSync(session, { recursive: true })
  writeFileSync(session + "/meta.json", JSON.stringify({ id }) + "\\n")
  writeFileSync(session + "/state.json", "{}\\n")
  writeFileSync(session + "/events.jsonl", "")
  process.exit(0)
}
const workspace = process.argv[process.argv.indexOf("--workspace") + 1]
const sessionId = process.argv[process.argv.indexOf("--session") + 1]
let input = ""
process.stdin.setEncoding("utf8")
process.stdin.on("data", chunk => { input += chunk })
process.stdin.on("end", () => {
  const request = JSON.parse(input.trim())
  if (request.params.prompt.includes("TOOL_HANG")) {
    console.log(JSON.stringify({ jsonrpc: "2.0", method: "agent.event", params: {
      type: "tool_started", call_id: "tool-1", name: "exec_command", arguments: {}
    } }))
    setInterval(() => {}, 10_000)
    return
  }
  writeFileSync(workspace + "/agent-change.txt", request.params.prompt)
  execFileSync("git", ["-C", workspace, "add", "."])
  execFileSync("git", ["-C", workspace, "commit", "-qm", "agent change"])
  const commit = execFileSync("git", ["-C", workspace, "rev-parse", "HEAD"], { encoding: "utf8" }).trim()
  if (request.params.prompt.includes("HANG")) setInterval(() => {}, 10_000)
  else console.log(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { value: { commit }, sessionId } }))
})
`)
  await chmod(fakePhi, 0o755)
  const requestPath = join(taskDir, "request.json")
  const worktreeRoot = join(base, "worktrees", taskId)
  await writeFile(requestPath, JSON.stringify({
    taskId,
    parentSessionId: "11111111-1111-4111-8111-111111111111",
    name,
    workflowPath,
    args,
    workspace: repository,
    home,
    taskDir,
    phi: fakePhi,
    startedAt: Date.now(),
    deadlineAt: Date.now() + 60_000,
    agentContext: {
      models: [{
        provider: "test",
        id: "test/model",
        model: "model",
        reasoning: [{ id: "low" }, { id: "high" }],
        default_reasoning: "low",
        service_tiers: [{ id: "default" }],
        default_service_tier: "default"
      }, {
        provider: "other",
        id: "other/model",
        model: "model",
        reasoning: [{ id: "high" }],
        default_reasoning: "high",
        service_tiers: [{ id: "priority" }],
        default_service_tier: "priority"
      }],
      model: "test/model",
      reasoning: "low",
      serviceTier: "default",
      allowShell: true,
      allowWrite: true,
      fullAccess: true,
      interactiveApprovals: true
    },
    git: {
      repoRoot: await realpath(repository),
      gitCommonDir: await realpath(join(repository, ".git")),
      startingCommit,
      workspaceRelative: "",
      repoName: "repository",
      repoHash: "test"
    },
    worktreeRoot,
    limits: { maxConcurrency: 2, maxAgents: 4, maxDurationMs: 60_000 },
    ...overrides
  }))
  return {
    base, home, repository, taskDir, requestPath, taskId, worktreeRoot,
    invocationsPath: join(base, "invocations.jsonl")
  }
}

function run(requestPath) {
  return new Promise(resolve => {
    const child = spawn(process.execPath, [runnerPath, requestPath], {
      stdio: ["ignore", "pipe", "pipe"]
    })
    let stderr = ""
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", chunk => { stderr += chunk })
    child.on("exit", code => resolve({ code, stderr }))
  })
}

async function waitFor(description, check) {
  for (let index = 0; index < 100; index += 1) {
    if (await check()) return
    await new Promise(resolve => setTimeout(resolve, 20))
  }
  assert.fail(`timed out waiting for ${description}`)
}

async function assertClean(fixtureValue) {
  const manifest = JSON.parse(await readFile(join(fixtureValue.taskDir, "worktrees.json")))
  assert.ok(manifest.entries.length > 0)
  assert.ok(manifest.entries.every(entry => entry.state === "cleaned"))
  assert.equal(git(fixtureValue.repository, "branch", "--list", `phi/${fixtureValue.taskId.slice(0, 8)}/*`), "")
  for (const entry of manifest.entries) {
    await assert.rejects(readFile(join(entry.path, "agent-change.txt")), /ENOENT/)
  }
  await assert.rejects(stat(fixtureValue.worktreeRoot), error => error?.code === "ENOENT")
  const children = (await readFile(join(fixtureValue.taskDir, "children.jsonl"), "utf8"))
    .trim().split("\n").map(line => JSON.parse(line))
  const childSessionId = children.find(entry => entry.childSessionId)?.childSessionId
  assert.ok(childSessionId)
  assert.ok((await stat(join(fixtureValue.home, "sessions", childSessionId))).isDirectory())
  return children
}

async function jsonLines(path) {
  return (await readFile(path, "utf8")).trim().split("\n").filter(Boolean).map(JSON.parse)
}

const header = `
import { agent } from "phi:workflow"
export const meta = { name: "managed-test", description: "managed test" }
`

test("runner persists bundled delegate results and child records", async () => {
  const source = await readFile(new URL("../workflows/delegate.js", import.meta.url), "utf8")
  const value = await fixture(source, {
    prompt: "DELEGATE",
    options: { label: "focused", schema: { type: "object" } }
  }, "delegate")
  try {
    const result = await run(value.requestPath)
    assert.equal(result.code, 0, result.stderr)
    const state = JSON.parse(await readFile(join(value.taskDir, "state.json")))
    const workflowResult = JSON.parse(await readFile(join(value.taskDir, "result.json")))
    const summary = JSON.parse(await readFile(join(value.taskDir, "summary.json")))
    const children = (await readFile(join(value.taskDir, "children.jsonl"), "utf8"))
      .trim().split("\n").map(line => JSON.parse(line))
    assert.equal(state.status, "completed")
    assert.equal(workflowResult.value.commit, git(value.repository, "rev-parse", "HEAD"))
    assert.deepEqual(
      summary.agents,
      { started: 1, running: 0, completed: 1, failed: 0, timedOut: 0 }
    )
    assert.ok(children.some(entry => entry.agentLabel === "focused" && entry.status === "created"))
    assert.ok(children.some(entry => entry.agentLabel === "focused" && entry.status === "completed"))
  } finally {
    await rm(value.base, { recursive: true, force: true })
  }
})

test("agent execution controls inherit and attenuate parent selection and authority", async () => {
  const value = await fixture(`${header}
export default async function () {
  await agent("PARENT", { label: "parent", timeout_ms: 5000, capabilities: "parent" })
  await agent("READ", { label: "read", timeout_ms: 5000, capabilities: "read-only" })
  return agent("WRITE", {
    label: "write",
    model: "other/model",
    reasoning: "high",
    timeout_ms: 5000,
    capabilities: "workspace-write"
  })
}`)
  try {
    const result = await run(value.requestPath)
    assert.equal(result.code, 0, result.stderr)
    const children = await jsonLines(join(value.taskDir, "children.jsonl"))
    const completed = Object.fromEntries(
      children.filter(entry => entry.status === "completed").map(entry => [entry.agentLabel, entry])
    )
    assert.equal(completed.parent.model, "test/model")
    assert.equal(completed.parent.reasoning, "low")
    assert.equal(completed.parent.capabilityProfile, "parent")
    assert.equal(completed.read.capabilityProfile, "read-only")
    assert.equal(completed.write.model, "other/model")
    assert.equal(completed.write.reasoning, "high")
    assert.equal(completed.write.serviceTier, "priority")
    assert.equal(completed.write.timeoutMs, 5000)

    const creates = (await jsonLines(value.invocationsPath))
      .filter(args => args.includes("internal-create-session"))
    const byLabel = Object.fromEntries(creates.map(args => [
      args[args.indexOf("--agent-label") + 1], args
    ]))
    assert.ok(byLabel.parent.includes("--yolo"))
    assert.ok(!byLabel.read.includes("--yolo"))
    assert.ok(!byLabel.read.includes("--allow-shell"))
    assert.ok(!byLabel.read.includes("--allow-write"))
    assert.ok(byLabel.write.includes("--allow-write"))
    assert.ok(byLabel.write.includes("--workspace-only"))
    assert.ok(!byLabel.write.includes("--allow-shell"))
    assert.ok(!byLabel.write.includes("--yolo"))
  } finally {
    await rm(value.base, { recursive: true, force: true })
  }
})

test("invalid model, reasoning, and capability escalation fail before child launch", async () => {
  const invalid = [
    ["unknown model", `{ model: "missing/model" }`],
    ["invalid reasoning", `{ model: "other/model", reasoning: "low" }`]
  ]
  for (const [name, options] of invalid) {
    const value = await fixture(`${header}
export default async function () { return agent("INVALID", ${options}) }
`)
    try {
      const result = await run(value.requestPath)
      assert.equal(result.code, 1, `${name}: ${result.stderr}`)
      await assert.rejects(readFile(value.invocationsPath), error => error?.code === "ENOENT")
    } finally {
      await rm(value.base, { recursive: true, force: true })
    }
  }

  for (const interactiveApprovals of [true, false]) {
    const value = await fixture(`${header}
export default async function () {
  return agent("ESCALATE", { capabilities: "workspace-write" })
}`)
    try {
      const request = JSON.parse(await readFile(value.requestPath))
      Object.assign(request.agentContext, {
        allowShell: false,
        allowWrite: false,
        fullAccess: false,
        interactiveApprovals
      })
      await writeFile(value.requestPath, JSON.stringify(request))
      const result = await run(value.requestPath)
      assert.equal(result.code, 1, result.stderr)
      await assert.rejects(readFile(value.invocationsPath), error => error?.code === "ENOENT")
    } finally {
      await rm(value.base, { recursive: true, force: true })
    }
  }
})

test("agent timeouts cover queued and child-session setup phases", async () => {
  const queued = await fixture(`${header}
export default async function () {
  const first = agent("HANG", { label: "running", timeout_ms: 500 })
  const second = agent("QUEUED", { label: "queued", timeout_ms: 30 })
  return Promise.allSettled([first, second])
}`, null, "managed-test", {
    limits: { maxConcurrency: 1, maxAgents: 4, maxDurationMs: 60_000 }
  })
  try {
    const result = await run(queued.requestPath)
    assert.equal(result.code, 0, result.stderr)
    const terminal = (await jsonLines(join(queued.taskDir, "children.jsonl")))
      .filter(entry => entry.status === "timed_out")
    assert.ok(terminal.some(entry => entry.agentLabel === "queued" && entry.launched === false))
    assert.ok(terminal.some(entry => entry.agentLabel === "running" && entry.launched === true))
  } finally {
    await rm(queued.base, { recursive: true, force: true })
  }

  const setup = await fixture(`${header}
export default async function () {
  try { await agent("SETUP", { label: "setup-hang", timeout_ms: 50 }) }
  catch (error) { return error.message }
}`)
  try {
    const result = await run(setup.requestPath)
    assert.equal(result.code, 0, result.stderr)
    const terminal = (await jsonLines(join(setup.taskDir, "children.jsonl")))
      .find(entry => entry.status === "timed_out")
    assert.equal(terminal.agentLabel, "setup-hang")
    assert.equal(terminal.launched, false)
    let invocations = []
    try { invocations = await jsonLines(setup.invocationsPath) } catch (error) {
      assert.equal(error?.code, "ENOENT")
    }
    assert.equal(invocations.filter(args => args.includes("rpc")).length, 0)
  } finally {
    await rm(setup.base, { recursive: true, force: true })
  }
})

test("agent timeouts cancel model and tool work and clean managed worktrees", {
  skip: process.platform === "win32"
}, async () => {
  for (const [label, prompt] of [["model", "HANG"], ["tool", "TOOL_HANG"]]) {
    const value = await fixture(`${header}
export default async function () {
  try {
    await agent(${JSON.stringify(prompt)}, {
      label: ${JSON.stringify(label)}, branch: ${JSON.stringify(label)}, timeout_ms: 2000
    })
  } catch (error) { return error.message }
}`)
    try {
      const result = await run(value.requestPath)
      assert.equal(result.code, 0, result.stderr)
      const children = await assertClean(value)
      assert.ok(children.some(entry =>
        entry.agentLabel === label && entry.status === "timed_out" && entry.launched === true
      ))
    } finally {
      await rm(value.base, { recursive: true, force: true })
    }
  }
})

test("agent timeout is clamped to the workflow deadline", async () => {
  const value = await fixture(`${header}
export default async function () {
  try { await agent("HANG", { timeout_ms: 5000 }) }
  catch (error) { return error.message }
}`, null, "managed-test", { deadlineAt: Date.now() + 1000 })
  try {
    const result = await run(value.requestPath)
    assert.equal(result.code, 0, result.stderr)
    const terminal = (await jsonLines(join(value.taskDir, "children.jsonl")))
      .find(entry => entry.status === "timed_out")
    assert.ok(terminal.timeoutMs > 0 && terminal.timeoutMs < 1000)
  } finally {
    await rm(value.base, { recursive: true, force: true })
  }
})

test("runner cleans managed worktrees after success and workflow failure", {
  skip: process.platform === "win32"
}, async () => {
  for (const failure of [false, true]) {
    const value = await fixture(`${header}
export default async function () {
  const result = await agent("COMMIT", { branch: "feature", schema: { type: "object" } })
  ${failure ? "throw new Error(\"workflow failed intentionally\")" : "return result"}
}`)
    try {
      const result = await run(value.requestPath)
      assert.equal(result.code, failure ? 1 : 0, result.stderr)
      const state = JSON.parse(await readFile(join(value.taskDir, "state.json")))
      assert.equal(state.status, failure ? "failed" : "completed")
      await assertClean(value)
    } finally {
      await rm(value.base, { recursive: true, force: true })
    }
  }
})

test("runner signal cleanup removes active managed worktrees", {
  skip: process.platform === "win32"
}, async () => {
  const value = await fixture(`${header}
export default async function () {
  return agent("HANG", { branch: "cancelled" })
}`)
  try {
    const child = spawn(process.execPath, [runnerPath, value.requestPath], {
      stdio: "ignore"
    })
    const manifestPath = join(value.taskDir, "worktrees.json")
    await waitFor("the child agent to start in its managed worktree", async () => {
      try {
        const manifest = JSON.parse(await readFile(manifestPath))
        const entry = manifest.entries[0]
        if (entry?.state !== "active") return false
        return (await stat(join(entry.path, "agent-change.txt"))).isFile()
      } catch {}
      return false
    })
    child.kill("SIGTERM")
    await new Promise(resolve => child.once("exit", resolve))
    const children = await assertClean(value)
    assert.ok(children.some(entry => entry.status === "cancelled"))
  } finally {
    await rm(value.base, { recursive: true, force: true })
  }
})

test("runner rejects a meta.name that differs from the requested name", async () => {
  const value = await fixture(`
export const meta = { name: "different", description: "wrong name" }
export default async function () { return null }
`)
  try {
    const result = await run(value.requestPath)
    assert.equal(result.code, 1, result.stderr)
    const state = JSON.parse(await readFile(join(value.taskDir, "state.json")))
    assert.equal(state.status, "failed")
    assert.match(state.error, /meta\.name must match requested name: managed-test/)
  } finally {
    await rm(value.base, { recursive: true, force: true })
  }
})
