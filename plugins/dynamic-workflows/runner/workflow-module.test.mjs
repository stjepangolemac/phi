import assert from "node:assert/strict"
import test from "node:test"
import { validateInputSchema, validateWorkflowModule } from "./workflow-module.mjs"

const inputSchema = {
  type: "object",
  properties: {
    review: {
      type: "object",
      properties: {
        files: { type: "array", items: { type: "string", minLength: 2 } }
      },
      required: ["files"],
      additionalProperties: false
    }
  },
  required: ["review"],
  additionalProperties: false
}

function workflow(schema = inputSchema) {
  return {
    meta: { name: "review", description: "Review files", inputSchema: schema },
    default: async () => null
  }
}

test("declared input schemas accept valid nested args", () => {
  const metadata = validateWorkflowModule(
    workflow(), "review", { review: { files: ["ok"] } }, true
  )
  assert.deepEqual(metadata.inputSchema, inputSchema)
  assert.doesNotThrow(() => validateWorkflowModule(workflow({
    type: "object",
    properties: { toString: { type: "string" } },
    required: ["toString"],
    additionalProperties: false
  }), "review", { toString: "safe own property" }, true))
})

test("declared input schemas report nested instance and schema paths", () => {
  assert.throws(
    () => validateWorkflowModule(workflow(), "review", { review: { files: [""] } }, true),
    /args at \/review\/files\/0 violate input schema at \/properties\/review\/properties\/files\/items\/minLength/
  )
})

test("workflows without schemas preserve arbitrary JSON args", () => {
  const module = workflow()
  delete module.meta.inputSchema
  assert.doesNotThrow(() => validateWorkflowModule(module, "review", ["arbitrary"], true))
})

test("ephemeral workflows derive identity from validated module metadata", () => {
  const metadata = validateWorkflowModule(workflow(), undefined, { review: { files: ["ok"] } }, true)
  assert.equal(metadata.name, "review")
  assert.equal(metadata.description, "Review files")
})

test("invalid and unsupported schema features have schema paths", () => {
  assert.throws(
    () => validateInputSchema({ type: "object", properties: { value: { $ref: "#/$defs/value" } } }),
    /input schema at \/properties\/value\/\$ref: unsupported keyword "\$ref"/
  )
  assert.throws(
    () => validateInputSchema({ type: "array", minItems: -1 }),
    /input schema at \/minItems: must be a non-negative integer/
  )
})
