import { describe, it, expect } from "vitest"
import {
  KEEP_EXISTING_SECRET_PLACEHOLDER,
  SECRET_MASK,
  buildSecretAwareUiSchema,
  relaxSecretRequirements,
  isSecretSchemaNode,
  maskSecretValues,
} from "./secretFields"

const schema = {
  type: "object",
  properties: {
    username: { type: "string" },
    password: { type: "string", format: "password" },
    apiToken: { type: "string", writeOnly: true },
    nested: {
      type: "object",
      properties: {
        clientSecret: { type: "string", writeOnly: true },
        port: { type: "integer" },
      },
    },
    keys: {
      type: "array",
      items: { type: "string", writeOnly: true },
    },
    users: {
      type: "array",
      items: {
        type: "object",
        properties: {
          name: { type: "string" },
          pass: { type: "string", format: "password" },
        },
      },
    },
  },
  allOf: [{ properties: { dbPassword: { type: "string", writeOnly: true } } }],
}

describe("secretFields helpers", () => {
  it("detects writeOnly and format: password nodes", () => {
    expect(isSecretSchemaNode({ writeOnly: true })).toBe(true)
    expect(isSecretSchemaNode({ format: "password" })).toBe(true)
    expect(isSecretSchemaNode({ type: "string" })).toBe(false)
    expect(isSecretSchemaNode(null)).toBe(false)
  })

  it("masks secret values recursively without mutating input", () => {
    const data = {
      username: "admin",
      password: "hunter2",
      apiToken: "tok",
      nested: { clientSecret: "cs", port: 8080 },
      keys: ["k1", "k2"],
      users: [{ name: "bob", pass: "pw" }],
      dbPassword: "db",
    }
    const masked = maskSecretValues(schema, data)
    expect(masked).toEqual({
      username: "admin",
      password: SECRET_MASK,
      apiToken: SECRET_MASK,
      nested: { clientSecret: SECRET_MASK, port: 8080 },
      keys: [SECRET_MASK, SECRET_MASK],
      users: [{ name: "bob", pass: SECRET_MASK }],
      dbPassword: SECRET_MASK,
    })
    expect(data.password).toBe("hunter2")
    expect(maskSecretValues(undefined, { a: 1 })).toEqual({ a: 1 })
  })

  it("builds a uiSchema with password widgets, preserving catalog uiSchema", () => {
    const ui = buildSecretAwareUiSchema(schema, {
      username: { "ui:placeholder": "admin" },
      password: { "ui:widget": "textarea" },
    })
    expect(ui.username).toEqual({ "ui:placeholder": "admin" })
    // Explicit catalog widget wins.
    expect(ui.password).toEqual({ "ui:widget": "textarea" })
    expect(ui.apiToken).toEqual({ "ui:widget": "password" })
    expect(ui.nested).toEqual({ clientSecret: { "ui:widget": "password" } })
    expect(ui.keys).toEqual({ items: { "ui:widget": "password" } })
    expect(ui.users).toEqual({
      items: { pass: { "ui:widget": "password" } },
    })
    expect(buildSecretAwareUiSchema(undefined, { a: 1 })).toEqual({ a: 1 })
  })

  it("adds a keep-existing placeholder to secret fields in edit mode", () => {
    const ui = buildSecretAwareUiSchema(schema, undefined, { editMode: true })
    expect(ui.apiToken).toEqual({
      "ui:widget": "password",
      "ui:placeholder": KEEP_EXISTING_SECRET_PLACEHOLDER,
    })
    expect(ui.username).toBeUndefined()
  })

  it("drops secret fields from required lists (recursively) for edit mode", () => {
    const relaxed = relaxSecretRequirements({
      type: "object",
      required: ["user", "token"],
      properties: {
        user: { type: "string" },
        token: { type: "string", writeOnly: true },
        nested: {
          type: "object",
          required: ["pw"],
          properties: { pw: { type: "string", format: "password" } },
        },
        list: {
          type: "array",
          items: {
            type: "object",
            required: ["k"],
            properties: { k: { type: "string", writeOnly: true } },
          },
        },
      },
      allOf: [
        {
          type: "object",
          required: ["s"],
          properties: { s: { writeOnly: true } },
        },
      ],
    })
    expect(relaxed.required).toEqual(["user"])
    expect(relaxed.properties.nested.required).toEqual([])
    expect(relaxed.properties.list.items.required).toEqual([])
    expect(relaxed.allOf[0].required).toEqual([])
    expect(relaxSecretRequirements(undefined)).toBeUndefined()
  })
})
