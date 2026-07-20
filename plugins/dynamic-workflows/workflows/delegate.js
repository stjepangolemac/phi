import { agent } from "phi:workflow"

export const meta = {
  name: "delegate",
  description: "Delegate one focused prompt to a durable child agent.",
  inputSchema: {
    type: "object",
    properties: {
      prompt: { type: "string", minLength: 1 },
      options: {
        type: "object",
        properties: {
          label: { type: "string", minLength: 1 },
          schema: { type: ["object", "boolean"] },
          branch: { type: "string", minLength: 1 },
          branch_off: { type: "string", minLength: 1 }
        },
        additionalProperties: false
      }
    },
    required: ["prompt"],
    additionalProperties: false
  }
}

export default async function ({ args }) {
  return agent(args.prompt, args.options ?? {})
}
