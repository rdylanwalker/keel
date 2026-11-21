import { describe, expect, it } from "vitest"
import { formatHashRoute, parseHashRoute } from "./routes"

describe("hash routes", () => {
  it("parses tabs and params", () => {
    expect(parseHashRoute("")).toEqual({ tab: "topology" })
    expect(parseHashRoute("#/providers")).toEqual({ tab: "providers" })
    expect(parseHashRoute("#bogus")).toEqual({ tab: "topology" })
    expect(parseHashRoute("#preview/42")).toEqual({
      tab: "preview",
      proposalId: "42",
    })
    expect(parseHashRoute("#preview/<script>")).toEqual({ tab: "preview" })
    expect(parseHashRoute("#catalog/vm")).toEqual({
      tab: "catalog",
      kind: "vm",
    })
    expect(parseHashRoute("#catalog/nope")).toEqual({ tab: "catalog" })
    expect(parseHashRoute("#preview/%E0%A4%A")).toEqual({ tab: "preview" })
  })

  it("formats routes", () => {
    expect(formatHashRoute({ tab: "preview", proposalId: "7" })).toBe(
      "#preview/7"
    )
    expect(formatHashRoute({ tab: "catalog", kind: "app" })).toBe(
      "#catalog/app"
    )
    expect(formatHashRoute({ tab: "providers" })).toBe("#providers")
  })
})
