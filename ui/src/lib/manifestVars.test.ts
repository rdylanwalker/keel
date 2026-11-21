import { describe, expect, it } from "vitest"

import {
  embeddedValueProblem,
  placeholderIsWholeScalar,
  scanManifestPlaceholders,
  validateManifestValues,
} from "./manifestVars"

const TEMPLATE = `apiVersion: apps/v1
kind: Deployment
metadata:
  name: \${appName}
spec:
  template:
    spec:
      containers:
        - name: app
          image: ghcr.io/acme/\${imageName}:\${tag:=latest}
          args:
            - \${firstArg}
          env:
            - name: GREETING
              value: \${greeting} # comment ok
`

describe("manifestVars (mirror of keel_core::render_manifest_template)", () => {
  it("detects whole-scalar vs embedded placeholders", () => {
    const usages = scanManifestPlaceholders(TEMPLATE)
    expect(usages).toEqual([
      { name: "appName", wholeScalar: true },
      { name: "imageName", wholeScalar: false },
      { name: "tag", wholeScalar: false },
      { name: "firstArg", wholeScalar: true },
      { name: "greeting", wholeScalar: true },
    ])
  })

  it("ignores invalid names and unterminated placeholders", () => {
    expect(scanManifestPlaceholders("a: ${1bad}\nb: ${open\nc: $x")).toEqual([])
  })

  it("mirrors placeholder_is_whole_scalar edge cases", () => {
    expect(placeholderIsWholeScalar("  key: ", "")).toBe(true)
    expect(placeholderIsWholeScalar("- ", "  ")).toBe(true)
    expect(placeholderIsWholeScalar("- - ", "")).toBe(true)
    expect(placeholderIsWholeScalar("", "")).toBe(false)
    expect(placeholderIsWholeScalar("key:", "")).toBe(false)
    expect(placeholderIsWholeScalar("key: ", "x")).toBe(false)
    expect(placeholderIsWholeScalar("key: ", " # note")).toBe(true)
    expect(placeholderIsWholeScalar("key: ", "#nospace")).toBe(false)
    expect(placeholderIsWholeScalar("-x ", "")).toBe(false)
  })

  it("rejects structural characters only for embedded values", () => {
    expect(embeddedValueProblem("nginx")).toBeNull()
    expect(embeddedValueProblem("a\nb")).toMatch(/newlines/)
    expect(embeddedValueProblem('a"b')).toMatch(/cannot contain/)
    expect(embeddedValueProblem("a: b")).toMatch(/': '/)
    expect(embeddedValueProblem("trailing:")).toMatch(/': '/)
    expect(embeddedValueProblem("-lead")).toMatch(/start with/)
    expect(embeddedValueProblem("trail ")).toMatch(/end with/)
  })

  it("validates form values against placement", () => {
    expect(
      validateManifestValues(TEMPLATE, {
        appName: "anything: goes\n---",
        imageName: "web#1",
        tag: "v1",
        firstArg: ["x"],
        greeting: { a: 1 },
      })
    ).toEqual({
      imageName: [expect.stringMatching(/cannot contain "#"/)],
    })
    expect(validateManifestValues(TEMPLATE, { tag: { a: 1 } })).toEqual({
      tag: [expect.stringMatching(/structured value/)],
    })
    expect(validateManifestValues(undefined, { a: 1 })).toEqual({})
    expect(validateManifestValues(TEMPLATE, undefined)).toEqual({})
    expect(
      validateManifestValues(TEMPLATE, { tag: null, imageName: 3 })
    ).toEqual({})
  })
})
