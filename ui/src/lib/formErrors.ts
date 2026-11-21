import type { ErrorSchema, RJSFValidationError } from "@rjsf/utils"

function humanList(values: unknown[]): string {
  const shown = values.slice(0, 6).map((v) => JSON.stringify(v))
  return values.length > 6 ? `${shown.join(", ")}, …` : shown.join(", ")
}

/**
 * Rewrites a single validation error into plain language. Errors use AJV's `name`/`params`
 * contract, produced by the eval-free validator in `@/components/rjsf/validator`.
 */
export function friendlyMessage(err: RJSFValidationError): string {
  const p = (err.params ?? {}) as Record<string, unknown>
  switch (err.name) {
    case "required":
      return "is required"
    case "minLength":
      return p.limit === 1
        ? "must not be empty"
        : `must be at least ${p.limit} characters`
    case "maxLength":
      return `must be at most ${p.limit} characters`
    case "pattern":
      return "has an invalid format"
    case "enum":
      return Array.isArray(p.allowedValues)
        ? `must be one of ${humanList(p.allowedValues)}`
        : "must be one of the allowed values"
    case "const":
      return `must be ${JSON.stringify(p.allowedValue)}`
    case "type":
      return `must be ${
        p.type === "integer"
          ? "a whole number"
          : p.type === "number"
            ? "a number"
            : p.type === "boolean"
              ? "true or false"
              : `of type ${String(p.type)}`
      }`
    case "minimum":
      return `must be ${p.comparison === ">" ? "greater than" : "at least"} ${p.limit}`
    case "maximum":
      return `must be ${p.comparison === "<" ? "less than" : "at most"} ${p.limit}`
    case "exclusiveMinimum":
      return `must be greater than ${p.limit}`
    case "exclusiveMaximum":
      return `must be less than ${p.limit}`
    case "format":
      return `must be a valid ${String(p.format)}`
    case "minItems":
      return `must have at least ${p.limit} item${p.limit === 1 ? "" : "s"}`
    case "maxItems":
      return `must have at most ${p.limit} items`
    case "uniqueItems":
      return "must not contain duplicate items"
    case "multipleOf":
      return `must be a multiple of ${p.multipleOf}`
    case "additionalProperties":
      return `has an unexpected field ${JSON.stringify(p.additionalProperty)}`
    case "oneOf":
      return "must match exactly one of the allowed options"
    case "anyOf":
      return "must match at least one of the allowed options"
    case "dependencies":
    case "dependentRequired":
      return `is required when ${String(p.property)} is set`
    default:
      return err.message ?? "is invalid"
  }
}

function labelFor(err: RJSFValidationError): string {
  if (
    err.name === "required" &&
    typeof err.params?.missingProperty === "string"
  ) {
    return err.title || err.params.missingProperty
  }
  if (err.title) return err.title
  const prop = (err.property ?? "").replace(/^\./, "")
  return prop || "Value"
}

/**
 * rjsf `transformErrors`: replaces raw validator text (`must have required property 'x'`,
 * `must match pattern "^…$"`) with friendly messages; the summary line names the field.
 */
export function transformErrors(
  errors: RJSFValidationError[]
): RJSFValidationError[] {
  return errors.map((err) => {
    const message = friendlyMessage(err)
    return { ...err, message, stack: `${labelFor(err)} ${message}` }
  })
}

/**
 * Converts dotted field paths (`a.b`) with messages into an rjsf `extraErrors` ErrorSchema
 * so server-side validation failures render next to the matching fields.
 */
export function toErrorSchema(
  fieldErrors: Record<string, string[]>
): ErrorSchema | undefined {
  const keys = Object.keys(fieldErrors)
  if (keys.length === 0) return undefined
  const root: Record<string, unknown> = {}
  for (const key of keys) {
    let node = root
    for (const seg of key.split(".").filter(Boolean)) {
      node = (node[seg] ??= {}) as Record<string, unknown>
    }
    const existing = (node.__errors as string[] | undefined) ?? []
    node.__errors = [...existing, ...fieldErrors[key]]
  }
  return root as ErrorSchema
}
