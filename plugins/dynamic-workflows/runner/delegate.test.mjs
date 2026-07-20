import assert from "node:assert/strict"
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import test from "node:test"
import { pathToFileURL } from "node:url"
import { prepareSource, validateWorkflowModule } from "./workflow-module.mjs"

const workflowPath = new URL("../workflows/delegate.js", import.meta.url)

async function loadDelegate() {
  const directory = await mkdtemp(join(tmpdir(), "phi-delegate-test-"))
  const apiPath = join(directory, "api.mjs")
  const modulePath = join(directory, "delegate.mjs")
  await writeFile(apiPath, `
export async function agent(prompt, options) {
  return { prompt, options }
}
`)
  const source = await readFile(workflowPath, "utf8")
  await writeFile(modulePath, prepareSource(source, pathToFileURL(apiPath).href))
  return {
    directory,
    module: await import(`${pathToFileURL(modulePath).href}?test=${Date.now()}`)
  }
}

test("bundled delegate forwards plain and structured agent requests", async () => {
  const loaded = await loadDelegate()
  try {
    const metadata = validateWorkflowModule(
      loaded.module, "delegate", { prompt: "Summarize this change." }, true
    )
    assert.equal(metadata.description, "Delegate one focused prompt to a durable child agent.")
    assert.deepEqual(
      await loaded.module.default({ args: { prompt: "Summarize this change." } }),
      { prompt: "Summarize this change.", options: {} }
    )

    const options = {
      label: "summary",
      schema: {
        type: "object",
        properties: { summary: { type: "string" } },
        required: ["summary"],
        additionalProperties: false
      },
      branch: "summary",
      branch_off: "main"
    }
    validateWorkflowModule(
      loaded.module, "delegate", { prompt: "Summarize.", options }, true
    )
    assert.deepEqual(
      await loaded.module.default({ args: { prompt: "Summarize.", options } }),
      { prompt: "Summarize.", options }
    )
  } finally {
    await rm(loaded.directory, { recursive: true, force: true })
  }
})

test("bundled delegate rejects missing prompts and unsupported options", async () => {
  const loaded = await loadDelegate()
  try {
    assert.throws(
      () => validateWorkflowModule(loaded.module, "delegate", { options: {} }, true),
      /args at \/prompt violate input schema at \/required/
    )
    assert.throws(
      () => validateWorkflowModule(loaded.module, "delegate", {
        prompt: "Do work.",
        options: { unsupported: true }
      }, true),
      /args at \/options\/unsupported violate input schema at \/properties\/options\/additionalProperties/
    )
  } finally {
    await rm(loaded.directory, { recursive: true, force: true })
  }
})
