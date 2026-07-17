import { log } from "phi:workflow"

export const meta = {
  name: "example",
  description: "Return the supplied arguments without launching an agent."
}

export default async function ({ args }) {
  log("Example workflow completed")
  return args
}
