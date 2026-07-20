import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import test from "node:test"

import { agent, configure } from "./api.mjs"
import { createWorktreeManager } from "./worktrees.mjs"

function git(root, ...args) {
  return execFileSync("git", ["-C", root, ...args], { encoding: "utf8" }).trim()
}

async function repository({ subdirectory = false } = {}) {
  const root = await mkdtemp(join(tmpdir(), "phi-worktrees-test-"))
  git(root, "init", "-q")
  git(root, "config", "user.name", "Phi Test")
  git(root, "config", "user.email", "phi@example.invalid")
  await writeFile(join(root, "base.txt"), "one\n")
  if (subdirectory) await mkdir(join(root, "nested", "workspace"), { recursive: true })
  git(root, "add", ".")
  git(root, "commit", "-qm", "base")
  return root
}

function manager(root, taskId, startingCommit, relative = "") {
  const manifests = []
  const events = []
  const worktreeRoot = `${root}-owned-${taskId}`
  return {
    manifests,
    events,
    worktreeRoot,
    value: createWorktreeManager({
      taskId,
      git: {
        repoRoot: root,
        startingCommit,
        workspaceRelative: relative
      },
      worktreeRoot,
      persist: async value => { manifests.push(structuredClone(value)) },
      progress: async (type, value) => { events.push({ type, ...value }) }
    })
  }
}

test("default branches use launch commit and preserve workspace subdirectory", async () => {
  const root = await repository({ subdirectory: true })
  try {
    const launchCommit = git(root, "rev-parse", "HEAD")
    await writeFile(join(root, "base.txt"), "two\n")
    git(root, "commit", "-qam", "later")
    const currentCommit = git(root, "rev-parse", "HEAD")
    const managed = manager(
      root,
      "11111111-1111-4111-8111-111111111111",
      launchCommit,
      join("nested", "workspace")
    )

    const prepared = await managed.value.prepare("feature one")
    const entry = managed.value.snapshot().entries[0]
    assert.equal(git(root, "rev-parse", entry.branch), launchCommit)
    assert.notEqual(launchCommit, currentCommit)
    assert.equal(prepared.workspace, join(entry.path, "nested", "workspace"))
    assert.match(prepared.promptContext, /feature one: phi\/11111111\//)

    await managed.value.finished("feature one", "completed")
    await managed.value.cleanup()
    assert.equal(git(root, "branch", "--list", entry.branch), "")
    assert.equal(managed.value.snapshot().entries[0].state, "cleaned")
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("external and completed managed branch_off refs integrate without collisions", async () => {
  const root = await repository()
  try {
    const launchCommit = git(root, "rev-parse", "HEAD")
    git(root, "branch", "external-base")
    const managed = manager(root, "22222222-2222-4222-8222-222222222222", launchCommit)

    const external = await managed.value.prepare("external child", "external-base")
    const externalEntry = managed.value.snapshot().entries[0]
    assert.equal(git(root, "rev-parse", externalEntry.branch), launchCommit)
    await writeFile(join(external.workspace, "external.txt"), "external\n")
    git(external.workspace, "add", ".")
    git(external.workspace, "commit", "-qm", "external change")
    await managed.value.finished("external child", "completed")
    const externalCommit = git(root, "rev-parse", externalEntry.branch)

    const integrated = await managed.value.prepare("integration", "external child")
    const integrationEntry = managed.value.snapshot().entries[1]
    assert.equal(git(root, "rev-parse", integrationEntry.branch), externalCommit)
    assert.match(integrated.promptContext, /external child: .*\n- integration:/)

    await managed.value.finished("integration", "completed")
    await managed.value.cleanup()
    assert.equal(git(root, "branch", "--list", "phi/22222222/*"), "")
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("parallel branches are isolated and invalid ownership requests are rejected", async () => {
  const root = await repository()
  try {
    const launchCommit = git(root, "rev-parse", "HEAD")
    const managed = manager(root, "33333333-3333-4333-8333-333333333333", launchCommit)
    const [left, right] = await Promise.all([
      managed.value.prepare("left"),
      managed.value.prepare("right")
    ])
    assert.notEqual(left.workspace, right.workspace)
    await writeFile(join(left.workspace, "only-left"), "left")
    assert.rejects(readFile(join(right.workspace, "only-left")), /ENOENT/)
    await assert.rejects(managed.value.prepare("left"), /duplicate managed branch/)
    await assert.rejects(managed.value.prepare("from-running", "right"), /not completed/)
    await assert.rejects(managed.value.prepare("from-missing", "definitely-not-a-ref"), /unknown revision|needed a single revision|ambiguous argument/i)
    await managed.value.cleanup()
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("pre-existing owned-looking refs and worktree paths are never adopted", async () => {
  const root = await repository()
  try {
    const launchCommit = git(root, "rev-parse", "HEAD")
    const taskId = "44444444-4444-4444-8444-444444444444"
    const managed = manager(root, taskId, launchCommit)
    const crypto = await import("node:crypto")
    const suffix = `collision-${crypto.createHash("sha256").update("collision").digest("hex").slice(0, 8)}`
    const branch = `phi/44444444/${suffix}`
    git(root, "branch", branch)
    await assert.rejects(managed.value.prepare("collision"), /already exists/)
    assert.equal(git(root, "rev-parse", branch), launchCommit)
    await managed.value.cleanup()
    assert.equal(git(root, "rev-parse", branch), launchCommit)

    const pathCollision = manager(
      root,
      "66666666-6666-4666-8666-666666666666",
      launchCommit
    )
    const sentinel = join(pathCollision.worktreeRoot, "pre-existing", "sentinel")
    await mkdir(join(pathCollision.worktreeRoot, "pre-existing"), { recursive: true })
    await writeFile(sentinel, "keep")
    await assert.rejects(pathCollision.value.prepare("feature"), /root already exists/)
    await pathCollision.value.cleanup()
    assert.equal(await readFile(sentinel, "utf8"), "keep")
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("agent and logical branch validation reject invalid combinations before spawning", async () => {
  let spawned = false
  configure({
    phi: "unused",
    workspace: "/unused",
    limits: { maxConcurrency: 1, maxAgents: 4 },
    worktrees: {
      prepare: async () => { spawned = true },
      finished: async () => {},
      promptContext: () => ""
    },
    isClosing: () => false,
    progress: () => {},
    childStarted: () => { spawned = true },
    childFinished: () => {}
  })
  await assert.rejects(agent("test", { branch_off: "main" }), /requires branch/)
  assert.equal(spawned, false)

  const manager = createWorktreeManager({
    taskId: "77777777-7777-4777-8777-777777777777",
    git: null,
    worktreeRoot: null,
    persist: async () => {},
    progress: async () => {}
  })
  await assert.rejects(manager.prepare("feature"), /requires a Git worktree workspace/)
  for (const name of ["", " leading", "trailing ", "line\nbreak", "x".repeat(129)]) {
    await assert.rejects(manager.prepare(name), /branch|cleanup/)
  }
})

test("agent timeout during managed-worktree setup prevents child launch", async () => {
  let spawned = false
  const records = []
  configure({
    phi: "unused",
    parentSessionId: "11111111-1111-4111-8111-111111111111",
    taskId: "22222222-2222-4222-8222-222222222222",
    workspace: "/unused",
    deadlineAt: Date.now() + 10_000,
    limits: { maxConcurrency: 1, maxAgents: 4, maxDurationMs: 10_000 },
    agentContext: {
      models: [{
        id: "test/model",
        reasoning: [{ id: "low" }],
        default_reasoning: "low",
        default_service_tier: "default"
      }],
      model: "test/model",
      reasoning: "low",
      serviceTier: "default",
      allowShell: false,
      allowWrite: false,
      fullAccess: false
    },
    worktrees: {
      prepare: async (_branch, _branchOff, signal) => new Promise((resolve, reject) => {
        const timer = setTimeout(() => resolve({
          workspace: "/managed",
          promptContext: "",
          branch: "phi/test/feature",
          worktreePath: "/managed"
        }), 5_000)
        signal.addEventListener("abort", () => {
          clearTimeout(timer)
          reject(signal.reason)
        }, { once: true })
      }),
      finished: async () => {},
      promptContext: () => ""
    },
    isClosing: () => false,
    progress: () => {},
    recordChild: async record => records.push(record),
    childStarted: () => { spawned = true },
    childFinished: () => {},
    setupStarted: () => {},
    setupFinished: () => {},
    agentStarted: () => {},
    agentFinished: () => {}
  })
  await assert.rejects(
    agent("test", { branch: "feature", timeout_ms: 10 }),
    /timed out/
  )
  assert.equal(spawned, false)
  assert.ok(records.some(record => record.status === "timed_out" && record.launched === false))
})
