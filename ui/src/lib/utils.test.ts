import { describe, it, expect, afterEach } from "vitest"
import { cn, jsonToYaml, isDemoMode } from "./utils"

describe("cn utility", () => {
  it("merges tailwind class names cleanly", () => {
    expect(cn("px-2 py-1", "bg-red-500", "px-4")).toBe("py-1 bg-red-500 px-4")
  })
})

describe("jsonToYaml utility", () => {
  it("formats primitives and objects into yaml syntax", () => {
    expect(jsonToYaml(null)).toBe("null\n")
    expect(jsonToYaml(undefined)).toBe("null\n")
    expect(jsonToYaml(true)).toBe("true\n")
    expect(jsonToYaml(42)).toBe("42\n")
    expect(jsonToYaml("hello")).toBe("hello\n")
    expect(jsonToYaml("key: val")).toBe('"key: val"\n')
    expect(jsonToYaml('quote " test')).toBe('"quote \\" test"\n')
    expect(jsonToYaml("backslash \\ test")).toBe('"backslash \\\\ test"\n')
    expect(jsonToYaml("line1\nline2")).toBe('"line1\\nline2"\n')
    expect(jsonToYaml(["a", "b"])).toBe("\n- a\n- b\n")
    expect(jsonToYaml([])).toBe("[]\n")
    expect(jsonToYaml({})).toBe("{}\n")
    expect(jsonToYaml({ name: "test", nested: { val: 1 } })).toContain(
      "name: test"
    )
  })
})

describe("isDemoMode utility", () => {
  const originalSearch = window.location.search

  afterEach(() => {
    window.history.replaceState(
      {},
      "",
      window.location.pathname + originalSearch
    )
  })

  it("returns false by default when demo param is absent or not true", () => {
    window.history.replaceState({}, "", "/?demo=false")
    expect(isDemoMode()).toBe(false)
    window.history.replaceState({}, "", "/")
    expect(isDemoMode()).toBe(false)
  })

  it("returns true when demo=true is in location search", () => {
    window.history.replaceState({}, "", "/?demo=true")
    expect(isDemoMode()).toBe(true)
  })
})
