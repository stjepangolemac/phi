import { readFile, writeFile, mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { pathToFileURL } from "node:url"
import { prepareSource, validateWorkflowModule } from "./workflow-module.mjs"

let input = ""
for await (const chunk of process.stdin) input += chunk

try {
  const request = JSON.parse(input)
  const source = typeof request.source === "string"
    ? request.source
    : await readFile(request.workflowPath, "utf8")
  const temporary = await mkdtemp(join(tmpdir(), "phi-workflow-inspect-"))
  try {
    const apiUrl = pathToFileURL(join(temporary, "api.mjs")).href
    await writeFile(join(temporary, "api.mjs"), [
      "const asyncNoop = async () => null",
      "const noop = () => null",
      "export const agent = asyncNoop, parallel = asyncNoop, batch = asyncNoop, pipeline = asyncNoop, phase = noop, log = noop, budget = noop"
    ].join("\n"))
    const modulePath = join(temporary, "workflow.mjs")
    await writeFile(modulePath, prepareSource(source, apiUrl))
    const module = await import(`${pathToFileURL(modulePath).href}?inspect=${Date.now()}`)
    const meta = validateWorkflowModule(module, request.name, request.args, request.validateArgs === true)
    process.stdout.write(JSON.stringify(meta))
  } finally {
    await rm(temporary, { recursive: true, force: true })
  }
} catch (error) {
  process.stderr.write(error?.stack ?? String(error))
  process.exitCode = 1
}
