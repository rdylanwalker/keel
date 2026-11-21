import { afterEach, describe, expect, it, vi } from "vitest"
import {
  ApiError,
  DemoModeError,
  errorMessage,
  fieldErrorsFromApiError,
  isIgnorableDemoError,
  parseApiError,
  visibleLoadError,
} from "./errors"

describe("parseApiError", () => {
  it("parses the JSON envelope and strips the correlation suffix", () => {
    const err = parseApiError(
      500,
      JSON.stringify({
        error: "Internal server error (correlation id: c-1)",
        correlationId: "c-1",
      })
    )
    expect(err).toBeInstanceOf(ApiError)
    expect(err.status).toBe(500)
    expect(err.message).toBe("Internal server error")
    expect(err.correlationId).toBe("c-1")
    expect(err.isServerError).toBe(true)
    expect(errorMessage(err, "x")).toBe(
      "Internal server error (reference: c-1)"
    )
  })

  it("parses schema validation details (camel and snake case)", () => {
    const err = parseApiError(
      400,
      JSON.stringify({
        error: "Schema validation failed",
        details: [
          { fieldPath: "/db/password", schemaPath: "/x", message: "too short" },
          { field_path: "/a~1b", schema_path: "/y", message: "bad" },
          { fieldPath: "/db/password", message: "weak" },
          "junk",
        ],
      })
    )
    expect(err.details).toHaveLength(3)
    expect(fieldErrorsFromApiError(err)).toEqual({
      "db.password": ["too short", "weak"],
      "a/b": ["bad"],
    })
    expect(errorMessage(err, "x")).toBe("Schema validation failed")
  })

  it("uses message field, plain text, and status fallbacks", () => {
    expect(parseApiError(409, '{"message":"conflict!"}').message).toBe(
      "conflict!"
    )
    expect(parseApiError(403, "{}").message).toBe(
      "You do not have permission to perform this action."
    )
    expect(parseApiError(400, "{not json").message).toBe("{not json")
    expect(parseApiError(422, "plain words").message).toBe("plain words")
    expect(parseApiError(502, "<html>bad gateway</html>").message).toBe(
      "The Keel server could not complete the request (HTTP 502)."
    )
    expect(parseApiError(418, "", "Teapot").message).toBe("Teapot")
    expect(parseApiError(418, undefined).message).toBe(
      "Request failed (HTTP 418)."
    )
  })

  it("errorMessage handles generic values", () => {
    expect(errorMessage(new Error("boom"), "fb")).toBe("boom")
    expect(errorMessage("weird", "fb")).toBe("fb")
    expect(errorMessage(new ApiError(404, "gone"), "fb")).toBe("gone")
    expect(fieldErrorsFromApiError(new Error("x"))).toEqual({})
  })
})

describe("demo error helpers", () => {
  afterEach(() => {
    window.history.replaceState(null, "", "/")
    vi.unstubAllEnvs()
  })

  it("always ignores DemoModeError", () => {
    expect(isIgnorableDemoError(new DemoModeError())).toBe(true)
    expect(visibleLoadError(new DemoModeError(), "fb")).toBeNull()
  })

  it("ignores 401s only in demo mode", () => {
    const unauthorized = new ApiError(401, "expired")
    expect(isIgnorableDemoError(unauthorized)).toBe(false)
    expect(visibleLoadError(unauthorized, "fb")).toBe("expired")
    window.history.replaceState(null, "", "/?demo=true")
    expect(isIgnorableDemoError(unauthorized)).toBe(true)
    expect(isIgnorableDemoError(new Error("API error (401): x"))).toBe(true)
    expect(isIgnorableDemoError(new ApiError(500, "x"))).toBe(false)
    expect(isIgnorableDemoError("str")).toBe(false)
  })
})
