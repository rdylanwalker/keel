import { describe, it, expect, afterEach } from "vitest"
import { parse as parseYaml } from "yaml"
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
    expect(jsonToYaml(() => {})).toBe("null\n")
    expect(jsonToYaml(true)).toBe("true\n")
    expect(jsonToYaml(42)).toBe("42\n")
    expect(jsonToYaml("hello")).toBe("hello\n")
    expect(jsonToYaml("key: val")).toBe('"key: val"\n')
    expect(jsonToYaml(["a", "b"])).toBe("- a\n- b\n")
    expect(jsonToYaml([])).toBe("[]\n")
    expect(jsonToYaml({})).toBe("{}\n")
    expect(jsonToYaml({ name: "test", nested: { val: 1 } })).toBe(
      "name: test\nnested:\n  val: 1\n"
    )
  })

  it("quotes strings that would otherwise change type or structure", () => {
    const tricky = {
      boolLike: "true",
      empty: "",
      numLike: "123",
      nullLike: "null",
      dash: "-",
      dashSpace: "- item",
      hash: "# comment",
      colon: "a: b",
      quote: 'quote " test',
      backslash: "back\\slash",
      multi: "line1\nline2",
      docSep: "x\n---\nkind: Secret",
    }
    const out = jsonToYaml(tricky)
    expect(out).toContain('boolLike: "true"')
    expect(out).toContain('empty: ""')
    expect(out).toContain('numLike: "123"')
    expect(out).toContain('nullLike: "null"')
    expect(out).toContain('dash: "-"')
    expect(out).toContain('dashSpace: "- item"')
    expect(out).toContain('hash: "# comment"')
    expect(out).toContain('colon: "a: b"')
    // Round-trips exactly (no injected keys/documents, no type coercion).
    expect(parseYaml(out)).toEqual(tricky)
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
    expect(isStackSshSupported({ provider: "azure", resourceKind: "vm" })).toBe(
      true
    )
    // Non-SSH providers never qualify, even when classified as a VM.
    expect(isStackSshSupported({ provider: "kind", resourceKind: "vm" })).toBe(
      false
    )
    expect(
      isStackSshSupported({ provider: "proxmox", resourceKind: "cluster" })
    ).toBe(false)
    expect(
      isStackSshSupported({ provider: "aws", resourceKind: "service" })
    ).toBe(false)
    expect(isStackSshSupported({ provider: "aws" })).toBe(false)
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
