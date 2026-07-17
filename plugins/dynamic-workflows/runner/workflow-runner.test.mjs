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

async function fixture(source) {
  const base = await mkdtemp(join(tmpdir(), "phi-workflow-runner-test-"))
  const repository = join(base, "repository")
  const taskDir = join(base, "task")
  const taskId = "66666666-6666-4666-8666-666666666666"
  await mkdir(repository)
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
import { writeFileSync } from "node:fs"
const workspace = process.argv[process.argv.indexOf("--workspace") + 1]
let input = ""
process.stdin.setEncoding("utf8")
process.stdin.on("data", chunk => { input += chunk })
process.stdin.on("end", () => {
  const request = JSON.parse(input.trim())
  writeFileSync(workspace + "/agent-change.txt", request.params.prompt)
  execFileSync("git", ["-C", workspace, "add", "."])
  execFileSync("git", ["-C", workspace, "commit", "-qm", "agent change"])
  const commit = execFileSync("git", ["-C", workspace, "rev-parse", "HEAD"], { encoding: "utf8" }).trim()
  if (request.params.prompt.includes("HANG")) setInterval(() => {}, 10_000)
  else console.log(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { value: { commit } } }))
})
`)
  await chmod(fakePhi, 0o755)
  const requestPath = join(taskDir, "request.json")
  const worktreeRoot = join(base, "worktrees", taskId)
  await writeFile(requestPath, JSON.stringify({
    taskId,
    name: "managed-test",
    workflowPath,
    args: null,
    workspace: repository,
    taskDir,
    phi: fakePhi,
    startedAt: Date.now(),
    git: {
      repoRoot: await realpath(repository),
      gitCommonDir: await realpath(join(repository, ".git")),
      startingCommit,
      workspaceRelative: "",
      repoName: "repository",
      repoHash: "test"
    },
    worktreeRoot,
    limits: { maxConcurrency: 2, maxAgents: 4 }
  }))
  return { base, repository, taskDir, requestPath, taskId, worktreeRoot }
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

async function assertClean(fixtureValue) {
  const manifest = JSON.parse(await readFile(join(fixtureValue.taskDir, "worktrees.json")))
  assert.ok(manifest.entries.length > 0)
  assert.ok(manifest.entries.every(entry => entry.state === "cleaned"))
  assert.equal(git(fixtureValue.repository, "branch", "--list", `phi/${fixtureValue.taskId.slice(0, 8)}/*`), "")
  for (const entry of manifest.entries) {
    await assert.rejects(readFile(join(entry.path, "agent-change.txt")), /ENOENT/)
  }
  await assert.rejects(stat(fixtureValue.worktreeRoot), error => error?.code === "ENOENT")
}

const header = `
import { agent } from "phi:workflow"
export const meta = { name: "managed-test", description: "managed test" }
`

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
    for (let index = 0; index < 100; index += 1) {
      try {
        const manifest = JSON.parse(await readFile(manifestPath))
        if (manifest.entries[0]?.state === "active") break
      } catch {}
      await new Promise(resolve => setTimeout(resolve, 20))
    }
    child.kill("SIGTERM")
    await new Promise(resolve => child.once("exit", resolve))
    await assertClean(value)
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
