import { spawn } from "node:child_process"
import { createHash } from "node:crypto"
import { mkdir, realpath, rm, rmdir } from "node:fs/promises"
import { existsSync } from "node:fs"
import { dirname, join, resolve } from "node:path"

function validateLogicalBranch(value) {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError("agent branch must be a non-empty string")
  }
  if (value.length > 128 || value !== value.trim() || value === "." || value === ".."
      || /[\u0000-\u001f\u007f]/.test(value)) {
    throw new TypeError(`invalid agent branch: ${JSON.stringify(value)}`)
  }
}

function slug(value) {
  const normalized = value
    .normalize("NFKD")
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^[.-]+|[.-]+$/g, "")
    .slice(0, 40)
  return normalized || "branch"
}

function shortHash(value) {
  return createHash("sha256").update(value).digest("hex").slice(0, 8)
}

function git(repoRoot, args, { allowFailure = false, signal } = {}) {
  if (signal?.aborted) return Promise.reject(signal.reason)
  return new Promise((resolve, reject) => {
    const child = spawn("git", ["-C", repoRoot, ...args], {
      stdio: ["ignore", "pipe", "pipe"]
    })
    let stdout = ""
    let stderr = ""
    let settled = false
    let abortReason = null
    let killTimer = null
    const finish = callback => {
      if (settled) return
      settled = true
      signal?.removeEventListener("abort", abort)
      clearTimeout(killTimer)
      callback()
    }
    const abort = () => {
      if (settled) return
      abortReason = signal.reason
      try { child.kill("SIGTERM") } catch {}
      killTimer = setTimeout(() => {
        try { child.kill("SIGKILL") } catch {}
      }, 2_000)
    }
    signal?.addEventListener("abort", abort, { once: true })
    child.stdout.setEncoding("utf8")
    child.stderr.setEncoding("utf8")
    child.stdout.on("data", chunk => { stdout += chunk })
    child.stderr.on("data", chunk => { stderr += chunk })
    child.on("error", error => finish(() => reject(abortReason ?? error)))
    child.on("exit", code => {
      const result = { code, stdout: stdout.trim(), stderr: stderr.trim() }
      finish(() => {
        if (abortReason) reject(abortReason)
        else if (code === 0 || allowFailure) resolve(result)
        else reject(new Error(result.stderr || `git ${args.join(" ")} exited with code ${code}`))
      })
    })
  })
}

export function createWorktreeManager(config) {
  const entries = new Map()
  const reservations = new Set()
  const operations = new Set()
  let cleaning = false
  let rootReady = null
  let rootOwned = false
  let persistQueue = Promise.resolve()

  function snapshot() {
    return {
      version: 1,
      taskId: config.taskId,
      repoRoot: config.git?.repoRoot ?? null,
      worktreeRoot: config.worktreeRoot,
      rootOwned,
      entries: [...entries.values()].map(entry => ({ ...entry }))
    }
  }

  function persist() {
    const value = snapshot()
    persistQueue = persistQueue.then(() => config.persist(value))
    return persistQueue
  }

  async function verifyRepository(signal) {
    if (!config.git?.gitCommonDir) return
    const result = await git(
      config.git.repoRoot, ["rev-parse", "--git-common-dir"], { signal }
    )
    const common = await realpath(resolve(config.git.repoRoot, result.stdout))
    if (common !== config.git.gitCommonDir) {
      throw new Error("Git repository identity changed during workflow")
    }
  }

  function ensureRoot() {
    if (!rootReady) {
      rootReady = (async () => {
        await mkdir(dirname(config.worktreeRoot), { recursive: true })
        try {
          await mkdir(config.worktreeRoot)
        } catch (error) {
          if (error?.code === "EEXIST") {
            throw new Error(`managed worktree root already exists: ${config.worktreeRoot}`)
          }
          throw error
        }
        rootOwned = true
        await persist()
      })()
    }
    return rootReady
  }

  async function resolveBase(branchOff, signal) {
    if (branchOff === undefined || branchOff === null) {
      return config.git.startingCommit
    }
    if (typeof branchOff !== "string" || branchOff.length === 0) {
      throw new TypeError("agent branch_off must be a non-empty string")
    }
    const managed = entries.get(branchOff)
    if (managed) {
      if (managed.agentStatus !== "completed") {
        throw new Error(`managed branch is not completed: ${branchOff}`)
      }
      return managed.commit
    }
    if (branchOff.startsWith(`phi/${config.taskId.slice(0, 8)}/`)) {
      throw new Error(`branch_off must use the completed managed logical name: ${branchOff}`)
    }
    const result = await git(config.git.repoRoot, [
      "rev-parse", "--verify", "--end-of-options", `${branchOff}^{commit}`
    ], { signal })
    return result.stdout
  }

  function promptContext(current = null) {
    const visible = [...entries.values()].filter(entry =>
      entry.logicalBranch === current || entry.agentStatus === "completed"
    )
    if (visible.length === 0) return ""
    const lines = visible.map(entry => {
      const marker = entry.logicalBranch === current ? " (current)" : ""
      return `- ${entry.logicalBranch}: ${entry.branch}${marker}`
    })
    return "Managed workflow branches:\n" + lines.join("\n") +
      "\nUse the actual Git refs above when merging managed branches. " +
      "Do not modify another worktree directly.\n\n"
  }

  async function prepareOperation(logicalBranch, branchOff, signal) {
    validateLogicalBranch(logicalBranch)
    signal?.throwIfAborted()
    if (!config.git) {
      throw new Error("agent branch requires a Git worktree workspace")
    }
    await verifyRepository(signal)
    await ensureRoot()
    signal?.throwIfAborted()
    if (entries.has(logicalBranch) || reservations.has(logicalBranch)) {
      throw new Error(`duplicate managed branch: ${logicalBranch}`)
    }
    reservations.add(logicalBranch)

    try {
      const suffix = `${slug(logicalBranch)}-${shortHash(logicalBranch)}`
      const branch = `phi/${config.taskId.slice(0, 8)}/${suffix}`
      const path = join(config.worktreeRoot, suffix)
      const baseCommit = await resolveBase(branchOff, signal)
      const existingBranch = await git(
        config.git.repoRoot,
        ["show-ref", "--verify", "--quiet", `refs/heads/${branch}`],
        { allowFailure: true, signal }
      )
      if (existingBranch.code === 0) {
        throw new Error(`managed branch already exists: ${branch}`)
      }
      if (existsSync(path)) {
        throw new Error(`managed worktree path already exists: ${path}`)
      }

      const entry = {
        logicalBranch,
        branch,
        path,
        baseCommit,
        state: "creating",
        branchCreated: false,
        agentStatus: "pending",
        commit: null
      }
      entries.set(logicalBranch, entry)
      await persist()
      try {
        await git(
          config.git.repoRoot,
          ["worktree", "add", "--detach", path, baseCommit],
          { signal }
        )
        entry.state = "worktree_created"
        await persist()
        signal?.throwIfAborted()
        await git(path, ["checkout", "-b", branch], { signal })
        entry.branchCreated = true
        entry.state = "active"
        await persist()
        await config.progress("worktree_created", {
          logicalBranch,
          branch,
          path,
          baseCommit
        })
      } catch (error) {
        entry.state = "cleanup_pending"
        entry.error = error.message
        await persist()
        throw error
      }

      const relative = config.git.workspaceRelative
      return {
        workspace: relative && relative !== "." ? join(path, relative) : path,
        promptContext: promptContext(logicalBranch),
        branch,
        logicalBranch,
        worktreePath: path
      }
    } finally {
      reservations.delete(logicalBranch)
    }
  }

  function prepare(logicalBranch, branchOff, signal) {
    if (cleaning) return Promise.reject(new Error("managed worktree cleanup has started"))
    const operation = prepareOperation(logicalBranch, branchOff, signal)
    operations.add(operation)
    operation.then(
      () => operations.delete(operation),
      () => operations.delete(operation)
    )
    return operation
  }

  async function finished(logicalBranch, status) {
    if (!logicalBranch) return
    const entry = entries.get(logicalBranch)
    if (!entry) return
    entry.agentStatus = status
    if (status === "completed") {
      const result = await git(config.git.repoRoot, ["rev-parse", "--verify", `${entry.branch}^{commit}`])
      entry.commit = result.stdout
    }
    await persist()
  }

  async function cleanup() {
    cleaning = true
    await Promise.allSettled([...operations])
    if (entries.size > 0) await verifyRepository()
    const errors = []
    const owned = [...entries.values()].reverse()
    for (const entry of owned) {
      if (entry.state === "cleaned") continue
      entry.state = "cleanup_pending"
      await persist()
      let ownsBranch = entry.branchCreated === true || entry.state === "active"
      if (!ownsBranch && existsSync(entry.path)) {
        const symbolic = await git(
          entry.path,
          ["symbolic-ref", "-q", "HEAD"],
          { allowFailure: true }
        )
        ownsBranch = symbolic.code === 0 && symbolic.stdout === `refs/heads/${entry.branch}`
      }
      const removed = await git(
        config.git.repoRoot,
        ["worktree", "remove", "--force", entry.path],
        { allowFailure: true }
      )
      if (rootOwned && existsSync(entry.path)) {
        await rm(entry.path, { recursive: true, force: true })
      }
      await git(config.git.repoRoot, ["worktree", "prune"], { allowFailure: true })
      const deleted = ownsBranch
        ? await git(
          config.git.repoRoot,
          ["branch", "-D", entry.branch],
          { allowFailure: true }
        )
        : { code: 0, stderr: "" }
      if (removed.code !== 0 && existsSync(entry.path)) {
        errors.push(`remove ${entry.logicalBranch}: ${removed.stderr || `exit ${removed.code}`}`)
      }
      const branchStillExists = await git(
        config.git.repoRoot,
        ["show-ref", "--verify", "--quiet", `refs/heads/${entry.branch}`],
        { allowFailure: true }
      )
      if (ownsBranch && branchStillExists.code === 0) {
        errors.push(`delete ${entry.logicalBranch}: ${deleted.stderr || `exit ${deleted.code}`}`)
      }
      if (!existsSync(entry.path) && (!ownsBranch || branchStillExists.code !== 0)) {
        entry.state = "cleaned"
        await config.progress("worktree_removed", {
          logicalBranch: entry.logicalBranch,
          branch: entry.branch,
          path: entry.path
        })
      }
      await persist()
    }
    if (config.git) {
      await git(config.git.repoRoot, ["worktree", "prune"], { allowFailure: true })
    }
    if (rootOwned) {
      await rmdir(config.worktreeRoot).catch(error => {
        if (error?.code !== "ENOENT" && error?.code !== "ENOTEMPTY") throw error
      })
    }
    await persistQueue
    if (errors.length > 0) {
      throw new Error(`managed worktree cleanup failed: ${errors.join("; ")}`)
    }
  }

  return {
    prepare,
    finished,
    cleanup,
    promptContext,
    snapshot
  }
}
