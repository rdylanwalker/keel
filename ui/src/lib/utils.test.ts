import { describe, it, expect, afterEach } from "vitest"
import {
  cn,
  jsonToYaml,
  isDemoMode,
  formatForgeProvider,
  isStackSshSupported,
} from "./utils"

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
    expect(jsonToYaml(() => {})).toContain("=>")
  })
})

describe("formatForgeProvider utility", () => {
  it("formats known and custom forge provider names", () => {
    expect(formatForgeProvider("forgejo")).toBe("Forgejo")
    expect(formatForgeProvider("gitea")).toBe("Gitea")
    expect(formatForgeProvider("github")).toBe("GitHub")
    expect(formatForgeProvider("gitlab")).toBe("GitLab")
    expect(formatForgeProvider("local")).toBe("Local Git")
    expect(formatForgeProvider("")).toBe("Local Git")
    expect(formatForgeProvider(undefined)).toBe("Local Git")
    expect(formatForgeProvider("bitbucket")).toBe("Bitbucket")
  })
})

describe("isStackSshSupported utility", () => {
  it("identifies SSH-capable VM stacks vs non-VM stacks", () => {
    expect(isStackSshSupported({ name: "vm-1", provider: "azure" })).toBe(true)
    expect(isStackSshSupported({ name: "vm-2", provider: "kind" })).toBe(false)
    expect(
      isStackSshSupported({ name: "management-cluster-core", provider: "" })
    ).toBe(false)
    expect(isStackSshSupported({ name: "x", type: "management" })).toBe(false)
    expect(isStackSshSupported({ name: "postgres-managed" })).toBe(false)
    expect(isStackSshSupported({ name: "canary-gateway-config" })).toBe(false)
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
