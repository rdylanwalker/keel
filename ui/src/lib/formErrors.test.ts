import { describe, expect, it } from "vitest"
import type { RJSFValidationError } from "@rjsf/utils"

import { friendlyMessage, toErrorSchema, transformErrors } from "./formErrors"

const e = (
  name: string,
  params: Record<string, unknown> = {},
  extra: Partial<RJSFValidationError> = {}
): RJSFValidationError => ({
  name,
  params,
  property: ".field",
  message: "raw",
  stack: "raw",
  ...extra,
})

describe("formErrors", () => {
  it("rewrites AJV messages into plain language", () => {
    expect(friendlyMessage(e("required", { missingProperty: "x" }))).toBe(
      "is required"
    )
    expect(friendlyMessage(e("minLength", { limit: 1 }))).toBe(
      "must not be empty"
    )
    expect(friendlyMessage(e("minLength", { limit: 3 }))).toBe(
      "must be at least 3 characters"
    )
    expect(friendlyMessage(e("maxLength", { limit: 9 }))).toBe(
      "must be at most 9 characters"
    )
    expect(friendlyMessage(e("pattern", { pattern: "^a$" }))).toBe(
      "has an invalid format"
    )
    expect(friendlyMessage(e("enum", { allowedValues: ["a", "b"] }))).toBe(
      'must be one of "a", "b"'
    )
    expect(
      friendlyMessage(e("enum", { allowedValues: [1, 2, 3, 4, 5, 6, 7] }))
    ).toMatch(/…$/)
    expect(friendlyMessage(e("enum"))).toBe("must be one of the allowed values")
    expect(friendlyMessage(e("const", { allowedValue: true }))).toBe(
      "must be true"
    )
    expect(friendlyMessage(e("type", { type: "integer" }))).toBe(
      "must be a whole number"
    )
    expect(friendlyMessage(e("type", { type: "number" }))).toBe(
      "must be a number"
    )
    expect(friendlyMessage(e("type", { type: "boolean" }))).toBe(
      "must be true or false"
    )
    expect(friendlyMessage(e("type", { type: "array" }))).toBe(
      "must be of type array"
    )
    expect(friendlyMessage(e("minimum", { limit: 1, comparison: ">=" }))).toBe(
      "must be at least 1"
    )
    expect(friendlyMessage(e("minimum", { limit: 1, comparison: ">" }))).toBe(
      "must be greater than 1"
    )
    expect(friendlyMessage(e("maximum", { limit: 5, comparison: "<=" }))).toBe(
      "must be at most 5"
    )
    expect(friendlyMessage(e("maximum", { limit: 5, comparison: "<" }))).toBe(
      "must be less than 5"
    )
    expect(friendlyMessage(e("exclusiveMinimum", { limit: 0 }))).toBe(
      "must be greater than 0"
    )
    expect(friendlyMessage(e("exclusiveMaximum", { limit: 9 }))).toBe(
      "must be less than 9"
    )
    expect(friendlyMessage(e("format", { format: "email" }))).toBe(
      "must be a valid email"
    )
    expect(friendlyMessage(e("minItems", { limit: 1 }))).toBe(
      "must have at least 1 item"
    )
    expect(friendlyMessage(e("maxItems", { limit: 2 }))).toBe(
      "must have at most 2 items"
    )
    expect(friendlyMessage(e("uniqueItems"))).toBe(
      "must not contain duplicate items"
    )
    expect(friendlyMessage(e("multipleOf", { multipleOf: 5 }))).toBe(
      "must be a multiple of 5"
    )
    expect(
      friendlyMessage(e("additionalProperties", { additionalProperty: "x" }))
    ).toBe('has an unexpected field "x"')
    expect(friendlyMessage(e("oneOf", { passingSchemas: null }))).toBe(
      "must match exactly one of the allowed options"
    )
    expect(friendlyMessage(e("anyOf"))).toBe(
      "must match at least one of the allowed options"
    )
    expect(
      friendlyMessage(
        e("dependencies", { property: "tls", missingProperty: "cert" })
      )
    ).toBe("is required when tls is set")
    expect(friendlyMessage(e("other"))).toBe("raw")
    expect(friendlyMessage({ stack: "s" })).toBe("is invalid")
  })

  it("prefixes the summary line with the field label", () => {
    const [req, pat, untitled] = transformErrors([
      e("required", { missingProperty: "adminEmail" }, { property: "" }),
      e("pattern", {}, { title: "Host Name" }),
      e("type", { type: "number" }, { property: "" }),
    ])
    expect(req.message).toBe("is required")
    expect(req.stack).toBe("adminEmail is required")
    expect(pat.stack).toBe("Host Name has an invalid format")
    expect(untitled.stack).toBe("Value must be a number")
  })

  it("builds nested extraErrors from dotted paths", () => {
    expect(toErrorSchema({})).toBeUndefined()
    expect(toErrorSchema({ "a.b": ["bad"], a: ["x"], "": ["root"] })).toEqual({
      __errors: ["root"],
      a: { __errors: ["x"], b: { __errors: ["bad"] } },
    })
  })
})
