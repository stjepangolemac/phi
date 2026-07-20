const supportedTypes = new Set([
  "array", "boolean", "integer", "null", "number", "object", "string"
])
const keywords = new Set([
  "$comment", "$id", "$schema", "additionalProperties", "allOf", "anyOf", "const",
  "default", "deprecated", "description", "enum", "examples", "exclusiveMaximum",
  "exclusiveMinimum", "items", "maxItems", "maxLength", "maxProperties", "maximum",
  "minItems", "minLength", "minProperties", "minimum", "multipleOf", "not", "oneOf",
  "pattern", "properties", "readOnly", "required", "title", "type", "uniqueItems",
  "writeOnly"
])

function pointer(path) {
  if (path.length === 0) return "<root>"
  return path.map(part => `/${String(part).replaceAll("~", "~0").replaceAll("/", "~1")}`).join("")
}

function schemaError(path, message) {
  throw new Error(`invalid workflow input schema at ${pointer(path)}: ${message}`)
}

function isJsonValue(value, seen = new Set()) {
  if (value === null || typeof value === "string" || typeof value === "boolean") return true
  if (typeof value === "number") return Number.isFinite(value)
  if (typeof value !== "object" || seen.has(value)) return false
  seen.add(value)
  const valid = Array.isArray(value)
    ? value.every(item => isJsonValue(item, seen))
    : Object.getPrototypeOf(value) === Object.prototype
      && Object.values(value).every(item => isJsonValue(item, seen))
  seen.delete(value)
  return valid
}

function nonnegativeInteger(schema, key, path) {
  if (schema[key] !== undefined && (!Number.isInteger(schema[key]) || schema[key] < 0)) {
    schemaError([...path, key], "must be a non-negative integer")
  }
}

export function validateInputSchema(schema, path = [], ancestors = new Set()) {
  if (typeof schema === "boolean") return
  if (schema === null || typeof schema !== "object" || Array.isArray(schema)) {
    schemaError(path, "must be an object or boolean")
  }
  if (Object.getPrototypeOf(schema) !== Object.prototype) {
    schemaError(path, "must be a plain JSON object")
  }
  if (ancestors.has(schema)) schemaError(path, "must not contain cycles")
  ancestors.add(schema)
  for (const key of Object.keys(schema)) {
    if (!keywords.has(key)) schemaError([...path, key], `unsupported keyword ${JSON.stringify(key)}`)
  }
  if (schema.$schema !== undefined && ![
    "https://json-schema.org/draft/2020-12/schema",
    "https://json-schema.org/draft/2020-12/schema#",
    "http://json-schema.org/draft-07/schema#",
    "https://json-schema.org/draft-07/schema#"
  ].includes(schema.$schema)) {
    schemaError([...path, "$schema"], "supports only JSON Schema draft 2020-12 and draft-07")
  }
  if (schema.type !== undefined) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type]
    if (types.length === 0 || types.some(type => !supportedTypes.has(type))) {
      schemaError([...path, "type"], "must contain supported JSON Schema types")
    }
    if (new Set(types).size !== types.length) schemaError([...path, "type"], "must not contain duplicates")
  }
  for (const key of ["$comment", "$id", "description", "title"]) {
    if (schema[key] !== undefined && typeof schema[key] !== "string") {
      schemaError([...path, key], "must be a string")
    }
  }
  for (const key of ["deprecated", "readOnly", "writeOnly"]) {
    if (schema[key] !== undefined && typeof schema[key] !== "boolean") {
      schemaError([...path, key], "must be a boolean")
    }
  }
  for (const key of ["const", "default", "examples"]) {
    if (schema[key] !== undefined && !isJsonValue(schema[key])) {
      schemaError([...path, key], "must be valid JSON without cycles or non-finite numbers")
    }
  }
  if (schema.enum !== undefined
      && (!Array.isArray(schema.enum) || schema.enum.length === 0
        || schema.enum.some(value => !isJsonValue(value))
        || schema.enum.some((value, index) =>
          schema.enum.slice(0, index).some(previous => deepEqual(previous, value))))) {
    schemaError([...path, "enum"], "must be a non-empty array of unique JSON values")
  }
  for (const key of ["allOf", "anyOf", "oneOf"]) {
    if (schema[key] !== undefined) {
      if (!Array.isArray(schema[key]) || schema[key].length === 0) {
        schemaError([...path, key], "must be a non-empty array of schemas")
      }
      schema[key].forEach((child, index) =>
        validateInputSchema(child, [...path, key, index], ancestors))
    }
  }
  if (schema.not !== undefined) validateInputSchema(schema.not, [...path, "not"], ancestors)
  if (schema.properties !== undefined) {
    if (schema.properties === null || typeof schema.properties !== "object"
        || Array.isArray(schema.properties)) {
      schemaError([...path, "properties"], "must be an object of schemas")
    }
    for (const [name, child] of Object.entries(schema.properties)) {
      validateInputSchema(child, [...path, "properties", name], ancestors)
    }
  }
  if (schema.required !== undefined
      && (!Array.isArray(schema.required)
        || schema.required.some(name => typeof name !== "string")
        || new Set(schema.required).size !== schema.required.length)) {
    schemaError([...path, "required"], "must be an array of unique strings")
  }
  if (schema.additionalProperties !== undefined) {
    validateInputSchema(schema.additionalProperties, [...path, "additionalProperties"], ancestors)
  }
  if (schema.items !== undefined) validateInputSchema(schema.items, [...path, "items"], ancestors)
  for (const key of ["minItems", "maxItems", "minLength", "maxLength", "minProperties", "maxProperties"]) {
    nonnegativeInteger(schema, key, path)
  }
  if (schema.uniqueItems !== undefined && typeof schema.uniqueItems !== "boolean") {
    schemaError([...path, "uniqueItems"], "must be a boolean")
  }
  if (schema.pattern !== undefined) {
    if (typeof schema.pattern !== "string") schemaError([...path, "pattern"], "must be a string")
    try { new RegExp(schema.pattern, "u") } catch (error) {
      schemaError([...path, "pattern"], `is not a valid regular expression: ${error.message}`)
    }
  }
  for (const key of ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"]) {
    if (schema[key] !== undefined
        && (typeof schema[key] !== "number" || !Number.isFinite(schema[key]))) {
      schemaError([...path, key], "must be a finite number")
    }
  }
  if (schema.multipleOf !== undefined
      && (typeof schema.multipleOf !== "number" || !Number.isFinite(schema.multipleOf)
        || schema.multipleOf <= 0)) {
    schemaError([...path, "multipleOf"], "must be a positive finite number")
  }
  ancestors.delete(schema)
}

function deepEqual(left, right) {
  if (left === right) return true
  if (typeof left !== "object" || left === null || typeof right !== "object" || right === null) {
    return false
  }
  if (Array.isArray(left) !== Array.isArray(right)) return false
  const leftKeys = Object.keys(left)
  const rightKeys = Object.keys(right)
  return leftKeys.length === rightKeys.length
    && leftKeys.every(key => Object.hasOwn(right, key) && deepEqual(left[key], right[key]))
}

function instanceType(value, type) {
  switch (type) {
    case "array": return Array.isArray(value)
    case "boolean": return typeof value === "boolean"
    case "integer": return typeof value === "number" && Number.isInteger(value)
    case "null": return value === null
    case "number": return typeof value === "number" && Number.isFinite(value)
    case "object": return value !== null && typeof value === "object" && !Array.isArray(value)
    case "string": return typeof value === "string"
  }
}

function violation(instancePath, schemaPath, message) {
  return { instancePath: pointer(instancePath), schemaPath: pointer(schemaPath), message }
}

function validateValue(value, schema, instancePath = [], schemaPath = []) {
  if (schema === true) return null
  if (schema === false) return violation(instancePath, schemaPath, "the schema rejects every value")
  if (schema.type !== undefined) {
    const types = Array.isArray(schema.type) ? schema.type : [schema.type]
    if (!types.some(type => instanceType(value, type))) {
      return violation(instancePath, [...schemaPath, "type"], `expected ${types.join(" or ")}`)
    }
  }
  if (schema.const !== undefined && !deepEqual(value, schema.const)) {
    return violation(instancePath, [...schemaPath, "const"], "must equal the declared constant")
  }
  if (schema.enum !== undefined && !schema.enum.some(item => deepEqual(value, item))) {
    return violation(instancePath, [...schemaPath, "enum"], "must equal one of the declared values")
  }
  for (const key of ["allOf", "anyOf", "oneOf"]) {
    if (schema[key] === undefined) continue
    const results = schema[key].map((child, index) =>
      validateValue(value, child, instancePath, [...schemaPath, key, index]))
    const matches = results.filter(result => result === null).length
    if (key === "allOf" && matches !== results.length) return results.find(Boolean)
    if (key === "anyOf" && matches === 0) {
      return violation(instancePath, [...schemaPath, key], "must match at least one schema")
    }
    if (key === "oneOf" && matches !== 1) {
      return violation(instancePath, [...schemaPath, key], `must match exactly one schema (matched ${matches})`)
    }
  }
  if (schema.not !== undefined && validateValue(value, schema.not, instancePath, [...schemaPath, "not"]) === null) {
    return violation(instancePath, [...schemaPath, "not"], "must not match this schema")
  }
  if (value !== null && typeof value === "object" && !Array.isArray(value)) {
    if (schema.minProperties !== undefined && Object.keys(value).length < schema.minProperties) {
      return violation(instancePath, [...schemaPath, "minProperties"], `must have at least ${schema.minProperties} properties`)
    }
    if (schema.maxProperties !== undefined && Object.keys(value).length > schema.maxProperties) {
      return violation(instancePath, [...schemaPath, "maxProperties"], `must have at most ${schema.maxProperties} properties`)
    }
    for (const name of schema.required ?? []) {
      if (!Object.hasOwn(value, name)) {
        return violation([...instancePath, name], [...schemaPath, "required"], "is required")
      }
    }
    const properties = schema.properties ?? {}
    for (const [name, item] of Object.entries(value)) {
      const child = Object.hasOwn(properties, name) ? properties[name] : undefined
      if (child !== undefined) {
        const error = validateValue(item, child, [...instancePath, name], [...schemaPath, "properties", name])
        if (error) return error
      } else if (schema.additionalProperties === false) {
        return violation([...instancePath, name], [...schemaPath, "additionalProperties"], "is not allowed")
      } else if (schema.additionalProperties !== undefined && schema.additionalProperties !== true) {
        const error = validateValue(item, schema.additionalProperties,
          [...instancePath, name], [...schemaPath, "additionalProperties"])
        if (error) return error
      }
    }
  }
  if (Array.isArray(value)) {
    if (schema.minItems !== undefined && value.length < schema.minItems) {
      return violation(instancePath, [...schemaPath, "minItems"], `must contain at least ${schema.minItems} items`)
    }
    if (schema.maxItems !== undefined && value.length > schema.maxItems) {
      return violation(instancePath, [...schemaPath, "maxItems"], `must contain at most ${schema.maxItems} items`)
    }
    if (schema.uniqueItems && value.some((item, index) =>
      value.slice(0, index).some(previous => deepEqual(previous, item)))) {
      return violation(instancePath, [...schemaPath, "uniqueItems"], "must contain unique items")
    }
    if (schema.items !== undefined) {
      for (let index = 0; index < value.length; index += 1) {
        const error = validateValue(value[index], schema.items,
          [...instancePath, index], [...schemaPath, "items"])
        if (error) return error
      }
    }
  }
  if (typeof value === "string") {
    const length = [...value].length
    if (schema.minLength !== undefined && length < schema.minLength) {
      return violation(instancePath, [...schemaPath, "minLength"], `must have at least ${schema.minLength} characters`)
    }
    if (schema.maxLength !== undefined && length > schema.maxLength) {
      return violation(instancePath, [...schemaPath, "maxLength"], `must have at most ${schema.maxLength} characters`)
    }
    if (schema.pattern !== undefined && !new RegExp(schema.pattern, "u").test(value)) {
      return violation(instancePath, [...schemaPath, "pattern"], `must match ${JSON.stringify(schema.pattern)}`)
    }
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    if (schema.minimum !== undefined && value < schema.minimum) {
      return violation(instancePath, [...schemaPath, "minimum"], `must be at least ${schema.minimum}`)
    }
    if (schema.maximum !== undefined && value > schema.maximum) {
      return violation(instancePath, [...schemaPath, "maximum"], `must be at most ${schema.maximum}`)
    }
    if (schema.exclusiveMinimum !== undefined && value <= schema.exclusiveMinimum) {
      return violation(instancePath, [...schemaPath, "exclusiveMinimum"], `must be greater than ${schema.exclusiveMinimum}`)
    }
    if (schema.exclusiveMaximum !== undefined && value >= schema.exclusiveMaximum) {
      return violation(instancePath, [...schemaPath, "exclusiveMaximum"], `must be less than ${schema.exclusiveMaximum}`)
    }
    if (schema.multipleOf !== undefined
        && Math.abs(value / schema.multipleOf - Math.round(value / schema.multipleOf)) > 1e-12) {
      return violation(instancePath, [...schemaPath, "multipleOf"], `must be a multiple of ${schema.multipleOf}`)
    }
  }
  return null
}

export function validateWorkflowModule(module, expectedName, args, validateArgs) {
  if (!module.meta || typeof module.meta.name !== "string"
      || typeof module.meta.description !== "string") {
    throw new Error("workflow must export meta with name and description")
  }
  if (module.meta.name !== expectedName) {
    throw new Error(`workflow meta.name must match requested name: ${expectedName}`)
  }
  if (typeof module.default !== "function") {
    throw new Error("workflow must default-export an async function")
  }
  const inputSchema = module.meta.inputSchema
  if (inputSchema !== undefined) {
    validateInputSchema(inputSchema)
    if (validateArgs) {
      const error = validateValue(args, inputSchema)
      if (error) {
        throw new Error(`workflow args at ${error.instancePath} violate input schema at ${error.schemaPath}: ${error.message}`)
      }
    }
  }
  return {
    name: module.meta.name,
    description: module.meta.description,
    ...(inputSchema === undefined ? {} : { inputSchema })
  }
}

export function prepareSource(source, apiUrl) {
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
