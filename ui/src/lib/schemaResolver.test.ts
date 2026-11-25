import { describe, it, expect } from "vitest"
import { SchemaResolver, asSchemaNode, hasSecretMarker } from "./schemaResolver"

const pw = { type: "string", writeOnly: true }
const fmtPw = { type: "string", format: "password" }

describe("asSchemaNode / hasSecretMarker", () => {
  it("accepts only plain objects", () => {
    expect(asSchemaNode({ a: 1 })).toEqual({ a: 1 })
    expect(asSchemaNode([1])).toBeUndefined()
    expect(asSchemaNode(null)).toBeUndefined()
    expect(asSchemaNode("x")).toBeUndefined()
  })

  it("detects writeOnly and format: password only", () => {
    expect(hasSecretMarker(pw)).toBe(true)
    expect(hasSecretMarker(fmtPw)).toBe(true)
    expect(hasSecretMarker({ writeOnly: false })).toBe(false)
    expect(hasSecretMarker({ type: "string" })).toBe(false)
    expect(hasSecretMarker(undefined)).toBe(false)
  })
})

describe("SchemaResolver.resolveRef", () => {
  const root = {
    $id: "https://example.test/app.json#",
    $defs: {
      pw,
      "a/b": { const: 1 },
      "c~d": { const: 2 },
      "sp ace": { const: 3 },
      list: [{ const: "first" }],
    },
    definitions: { legacy: fmtPw },
    properties: {
      anchored: { $anchor: "tok", type: "string" },
      data: { default: { $anchor: "notASchema" } },
    },
  }
  const r = new SchemaResolver(root)

  it("resolves the root and JSON pointers, including ~0/~1 escapes and percent-encoding", () => {
    expect(r.resolveRef("#")).toBe(root)
    expect(r.resolveRef("#/$defs/pw")).toBe(pw)
    expect(r.resolveRef("#/definitions/legacy")).toBe(fmtPw)
    expect(r.resolveRef("#/$defs/a~1b")).toEqual({ const: 1 })
    expect(r.resolveRef("#/$defs/c~0d")).toEqual({ const: 2 })
    expect(r.resolveRef("#/$defs/sp%20ace")).toEqual({ const: 3 })
    expect(r.resolveRef("#/$defs/list/0")).toEqual({ const: "first" })
  })

  it("resolves anchors but never inside data keywords", () => {
    expect(r.resolveRef("#tok")).toBe(root.properties.anchored)
    expect(r.resolveRef("#notASchema")).toBeUndefined()
  })

  it("resolves the root $id plus a fragment, and the bare $id", () => {
    expect(r.resolveRef("https://example.test/app.json#/$defs/pw")).toBe(pw)
    expect(r.resolveRef("https://example.test/app.json")).toBe(root)
  })

  it("never resolves remote, relative, malformed or dangling references", () => {
    expect(r.resolveRef("https://evil.test/x.json#/$defs/pw")).toBeUndefined()
    expect(r.resolveRef("other.json#/$defs/pw")).toBeUndefined()
    expect(r.resolveRef("https://example.test/app.jsonx")).toBeUndefined()
    expect(r.resolveRef("#/$defs/missing")).toBeUndefined()
    expect(r.resolveRef("#/$defs/list/x")).toBeUndefined()
    expect(r.resolveRef("#/$defs/pw/type/deeper")).toBeUndefined()
    expect(r.resolveRef("#%E0%A4%A")).toBeUndefined()
  })

  it("supports a root `id` and no id at all", () => {
    expect(
      new SchemaResolver({ id: "urn:x", $defs: { pw } }).resolveRef(
        "urn:x#/$defs/pw"
      )
    ).toBe(pw)
    expect(new SchemaResolver({ $defs: { pw } }).resolveRef("urn:x#/a")).toBe(
      undefined
    )
    expect(new SchemaResolver({ $id: "urn:y" }).resolveRef("urn:y")).toEqual({
      $id: "urn:y",
    })
  })
})

describe("SchemaResolver.expand / isSecret", () => {
  it("follows $ref, combinators, conditionals and dependent schemas", () => {
    const viaRef = { $ref: "#/$defs/pw" }
    const r = new SchemaResolver({ $defs: { pw } })
    expect(r.isSecret([viaRef])).toBe(true)
    for (const kw of ["allOf", "anyOf", "oneOf"]) {
      expect(r.isSecret([{ [kw]: [{ type: "string" }, fmtPw] }])).toBe(true)
    }
    for (const kw of ["if", "then", "else"]) {
      expect(r.isSecret([{ [kw]: pw }])).toBe(true)
    }
    expect(r.isSecret([{ dependentSchemas: { other: pw } }])).toBe(true)
    expect(
      r.isSecret([{ dependencies: { other: pw, listForm: ["a", "b"] } }])
    ).toBe(true)
    expect(r.isSecret([{ type: "string" }])).toBe(false)
  })

  it("is cycle-safe and deduplicates nodes", () => {
    const root: Record<string, unknown> = {
      $defs: { a: { $ref: "#/$defs/b" }, b: { $ref: "#/$defs/a" } },
    }
    const r = new SchemaResolver(root)
    expect(r.isSecret([{ $ref: "#/$defs/a" }])).toBe(false)
    const shared = { type: "string" }
    expect(r.expand([shared, shared, { allOf: [shared] }])).toHaveLength(2)
  })

  it("ignores unresolvable refs and non-object entries", () => {
    const r = new SchemaResolver({})
    expect(r.expand([{ $ref: "#/nope" }, null, "x", [1]])).toHaveLength(1)
  })
})

describe("SchemaResolver.isDeclaredNotSecret", () => {
  const r = new SchemaResolver({})
  it("is true only for writeOnly: false with no secret branch", () => {
    expect(r.isDeclaredNotSecret([{ writeOnly: false }])).toBe(true)
    expect(r.isDeclaredNotSecret([{ writeOnly: false, anyOf: [fmtPw] }])).toBe(
      false
    )
    expect(r.isDeclaredNotSecret([{ type: "string" }])).toBe(false)
  })
})

describe("SchemaResolver.propertySchemas", () => {
  const root = {
    properties: { known: { type: "string" } },
    patternProperties: { "^tok_": pw, "(": { type: "number" } },
    additionalProperties: { type: "boolean" },
  }
  const r = new SchemaResolver(root)

  it("collects properties and every matching pattern", () => {
    const set = r.propertySchemas([root], "known")
    // "known" also matches the invalid "(" pattern, which over-approximates like the server.
    expect(set).toContainEqual({ type: "string" })
    expect(set).toContainEqual({ type: "number" })
    expect(r.isSecret(r.propertySchemas([root], "tok_api"))).toBe(true)
  })

  it("falls back to additionalProperties only when nothing matched", () => {
    const strict = {
      properties: { a: { type: "string" } },
      additionalProperties: pw,
    }
    const rs = new SchemaResolver(strict)
    expect(rs.isSecret(rs.propertySchemas([strict], "a"))).toBe(false)
    expect(rs.isSecret(rs.propertySchemas([strict], "zzz"))).toBe(true)
  })

  it("drops boolean subschemas and finds properties through combinators", () => {
    const s = {
      additionalProperties: true,
      allOf: [{ properties: { nested: pw } }],
    }
    const rs = new SchemaResolver(s)
    expect(rs.propertySchemas([s], "other")).toEqual([])
    expect(rs.isSecret(rs.propertySchemas([s], "nested"))).toBe(true)
  })
})

describe("SchemaResolver.itemSchemas", () => {
  const r = new SchemaResolver({})

  it("handles prefixItems with an index", () => {
    const s = { prefixItems: [{ type: "string" }, pw], items: { const: 1 } }
    expect(r.isSecret(r.itemSchemas([s], 1))).toBe(true)
    expect(r.itemSchemas([s], 0)).toEqual([{ type: "string" }])
    // Beyond prefixItems the 2020-12 `items` keyword applies (same as the server).
    expect(r.itemSchemas([s], 5)).toEqual([{ const: 1 }])
    const secretTail = { prefixItems: [{ type: "string" }], items: pw }
    expect(r.isSecret(r.itemSchemas([secretTail], 3))).toBe(true)
  })

  it("handles draft-7 tuple items and additionalItems", () => {
    const s = { items: [{ type: "string" }], additionalItems: pw }
    expect(r.itemSchemas([s], 0)).toEqual([{ type: "string" }])
    expect(r.isSecret(r.itemSchemas([s], 3))).toBe(true)
    expect(r.itemSchemas([{ items: [{ type: "string" }] }], 3)).toEqual([])
  })

  it("handles a single items schema and the any-item form", () => {
    expect(r.isSecret(r.itemSchemas([{ items: pw }], 7))).toBe(true)
    const s = {
      prefixItems: [{ type: "string" }],
      items: [{ type: "number" }],
      additionalItems: pw,
    }
    const any = r.itemSchemas([s])
    expect(any).toContainEqual({ type: "string" })
    expect(any).toContainEqual({ type: "number" })
    expect(r.isSecret(any)).toBe(true)
    expect(r.itemSchemas([{ items: pw }])).toEqual([pw])
    expect(r.itemSchemas([{ type: "array" }])).toEqual([])
  })
})

describe("SchemaResolver path helpers", () => {
  const root = {
    $defs: { cred: { type: "object", properties: { token: pw } } },
    properties: {
      auth: { $ref: "#/$defs/cred" },
      users: {
        type: "array",
        items: {
          type: "object",
          properties: { password: fmtPw, name: { type: "string" } },
        },
      },
      matrix: { type: "array", items: { type: "array", items: pw } },
      blob: pw,
    },
  }
  const r = new SchemaResolver(root)

  it("schemasAtPath walks properties, refs and [] segments", () => {
    expect(r.schemasAtPath("")).toEqual([root])
    expect(r.isSecret(r.schemasAtPath("auth.token"))).toBe(true)
    expect(r.isSecret(r.schemasAtPath("users[].password"))).toBe(true)
    expect(r.isSecret(r.schemasAtPath("users[].name"))).toBe(false)
    expect(r.isSecret(r.schemasAtPath("matrix[][]"))).toBe(true)
    expect(r.schemasAtPath("missing.deeper")).toEqual([])
  })

  it("isSecretPath is true inside a secret field or an enclosing secret", () => {
    expect(r.isSecretPath("auth.token")).toBe(true)
    expect(r.isSecretPath("blob.anything.below")).toBe(true)
    expect(r.isSecretPath("users[].password")).toBe(true)
    expect(r.isSecretPath("matrix[][]")).toBe(true)
    expect(r.isSecretPath("users[].name")).toBe(false)
    expect(r.isSecretPath("missing.deeper")).toBe(false)
    expect(r.isSecretPath("")).toBe(false)
  })

  it("treats everything as secret under a secret root", () => {
    expect(new SchemaResolver(pw).isSecretPath("any.path")).toBe(true)
  })
})
