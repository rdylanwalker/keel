import { afterEach, describe, expect, it, vi } from "vitest"
import { parse } from "yaml"
import {
  getClosestMatchingOption,
  type RJSFSchema,
  type RJSFValidationError,
} from "@rjsf/utils"

import { transformErrors } from "@/lib/formErrors"
import { createValidator, toJsonInstance } from "./validator"

const validator = createValidator()

// Node's fs is loaded through a non-literal specifier so the browser tsconfig needs no node types.
const fsModule = "node:fs"
const { readFileSync } = (await import(/* @vite-ignore */ fsModule)) as {
  readFileSync: (file: URL, encoding: "utf8") => string
}

/** Real catalog schemas straight from `deploy/catalog` (loaded at runtime from Git in prod). */
function catalogSchema(entry: string): RJSFSchema {
  // Bind import.meta.url first so Vite's `new URL(…, import.meta.url)` asset rewrite skips it.
  const here = import.meta.url
  const file = new URL(`../../../../deploy/catalog/${entry}/catalog.yaml`, here)
  const doc = parse(readFileSync(file, "utf8")) as {
    spec: { schema: RJSFSchema }
  }
  return doc.spec.schema
}

const byName = (errors: RJSFValidationError[], name: string) =>
  errors.filter((e) => e.name === name)

describe("InterpretingValidator (eval-free rjsf validator)", () => {
  afterEach(() => vi.unstubAllGlobals())

  it("validates real catalog schemas with eval and Function disabled (CSP guard)", () => {
    const boom = () => {
      throw new EvalError("Refused to evaluate a string as JavaScript (CSP)")
    }
    vi.stubGlobal("eval", boom)
    vi.stubGlobal("Function", boom)
    expect(() => Function("return 1")).toThrow(EvalError)

    const v = createValidator<Record<string, unknown>>()
    const authentik = catalogSchema("apps/authentik")
    const ok = v.validateFormData(
      {
        domain: "auth.example.com",
        adminEmail: "admin@example.com",
        secretKey: "x".repeat(32),
        replicas: 2,
      },
      authentik
    )
    expect(ok.errors).toEqual([])

    const bad = v.validateFormData(
      {
        domain: "not a host!",
        adminEmail: "nope",
        secretKey: "short",
        replicas: 0,
      },
      authentik
    )
    expect(bad.errors.map((e) => `${e.property}:${e.name}`).sort()).toEqual([
      ".adminEmail:format",
      ".domain:format",
      ".replicas:minimum",
      ".secretKey:minLength",
    ])

    const eks = catalogSchema("infra/aws-eks-cluster")
    const eksErrors = v.validateFormData(
      {
        clusterName: "Bad_Name",
        kubernetesVersion: "0.1",
        desiredNodeCount: 2,
      },
      eks
    ).errors
    expect(byName(eksErrors, "pattern")[0]).toMatchObject({
      property: ".clusterName",
      params: { pattern: "^[a-z0-9-]+$" },
    })
    expect(byName(eksErrors, "enum")[0].params.allowedValues).toContain("1.35")
    expect(
      byName(eksErrors, "required").map((e) => e.params.missingProperty)
    ).toEqual(["region", "nodeInstanceType"])
    expect(
      validator.isValid(eks.properties!.clusterName as RJSFSchema, "ok-1", eks)
    ).toBe(true)
  })

  it("validates the self-service Rauthy catalog app secrets and patterns", () => {
    const rauthy = catalogSchema("apps/rauthy")
    const valid = {
      hostname: "auth-acme.keel.local",
      publicPortSuffix: ":8888",
      adminEmail: "admin@keel.local",
      logLevel: "info",
      storageSize: "1Gi",
      encKeyId: "keel1",
      encKey: "q2Vz7mU0ZcV1x7pYb7Yk2r9mD0aH3sW5tL8nQ4eR6fA=",
      hiqliteSecretRaft: "raftSecretSampleValue-0123456789",
      hiqliteSecretApi: "apiSecretSampleValue-9876543210",
      adminPassword: "Sample-Admin#Passw0rd!",
    }
    expect(validator.validateFormData(valid, rauthy).errors).toEqual([])

    const bad = validator.validateFormData(
      {
        ...valid,
        encKey: "not-base64",
        hiqliteSecretRaft: "short",
        adminPassword: 'has "quotes" and spaces',
        publicPortSuffix: "8888",
      },
      rauthy
    )
    expect(bad.errors.map((e) => `${e.property}:${e.name}`).sort()).toEqual([
      ".adminPassword:pattern",
      ".encKey:pattern",
      ".hiqliteSecretRaft:pattern",
      ".publicPortSuffix:pattern",
    ])

    // `$` would be re-substituted by Flux postBuild in the decrypted Secret.
    const dollar = validator.validateFormData(
      { ...valid, adminPassword: "has-${dollar}-1234567" },
      rauthy
    )
    expect(dollar.errors.map((e) => `${e.property}:${e.name}`)).toEqual([
      ".adminPassword:pattern",
    ])

    // Secrets are never defaulted: an empty form reports every secret as missing.
    const missing = validator.validateFormData({}, rauthy).errors
    expect(
      byName(missing, "required").map((e) => e.params.missingProperty)
    ).toEqual(
      expect.arrayContaining([
        "hostname",
        "encKey",
        "hiqliteSecretRaft",
        "hiqliteSecretApi",
        "adminPassword",
      ])
    )
    for (const field of [
      "encKey",
      "hiqliteSecretRaft",
      "hiqliteSecretApi",
      "adminPassword",
    ]) {
      const prop = rauthy.properties![field] as RJSFSchema
      expect(prop.format).toBe("password")
      expect(prop.default).toBeUndefined()
    }
    // No shared default hostname: two tenants with defaults would collide on the Gateway.
    expect((rauthy.properties!.hostname as RJSFSchema).default).toBeUndefined()
    expect((rauthy.properties!.adminForceMfa as RJSFSchema).default).toBe(false)
  })

  it("produces AJV-compatible required errors with titles, errorSchema and friendly text", () => {
    const schema: RJSFSchema = {
      type: "object",
      required: ["name"],
      properties: { name: { type: "string", title: "Service Name" } },
    }
    const { errors, errorSchema } = validator.validateFormData(
      { name: undefined },
      schema,
      undefined,
      transformErrors
    )
    expect(errors).toHaveLength(1)
    expect(errors[0]).toMatchObject({
      name: "required",
      property: "name",
      params: { missingProperty: "name" },
      schemaPath: "#/required",
      title: "Service Name",
      message: "is required",
      stack: "Service Name is required",
    })
    expect(errorSchema).toEqual({ name: { __errors: ["is required"] } })
  })

  it("maps keyword params and messages like AJV", () => {
    const schema: RJSFSchema = {
      type: "object",
      additionalProperties: false,
      properties: {
        n: { type: "integer", exclusiveMinimum: 0, multipleOf: 2 },
        s: { type: "string", maxLength: 2 },
        arr: { type: "array", items: { type: "string" }, uniqueItems: true },
        c: { const: "x" },
      },
    }
    const { errors } = validator.validateFormData(
      { n: 3, s: "abc", arr: ["a", "a", 1], c: "y", extra: true },
      schema
    )
    const get = (name: string) => byName(errors, name)[0]
    expect(get("multipleOf")).toMatchObject({
      property: ".n",
      params: { multipleOf: 2 },
      message: "must be multiple of 2",
    })
    expect(get("maxLength")).toMatchObject({
      property: ".s",
      params: { limit: 2 },
      message: "must NOT have more than 2 characters",
    })
    expect(get("type")).toMatchObject({
      property: ".arr.2",
      params: { type: "string" },
      message: "must be string",
    })
    expect(get("uniqueItems").property).toBe(".arr")
    expect(get("const").params).toEqual({ allowedValue: "x" })
    expect(get("additionalProperties")).toMatchObject({
      property: "",
      params: { additionalProperty: "extra" },
    })
    // wrapper keywords (properties/items/$ref) never surface as errors
    expect(get("properties")).toBeUndefined()
    expect(get("items")).toBeUndefined()

    const { errors: minErrs } = validator.validateFormData({ n: 0 }, schema)
    expect(byName(minErrs, "exclusiveMinimum")[0]).toMatchObject({
      params: { comparison: ">", limit: 0 },
      message: "must be > 0",
    })
  })

  it("reports nested object and array paths and builds nested errorSchema", () => {
    const schema: RJSFSchema = {
      type: "object",
      properties: {
        db: {
          type: "object",
          required: ["host"],
          properties: {
            host: { type: "string", format: "hostname" },
            port: { type: "integer", minimum: 1, maximum: 65535 },
          },
        },
        nodes: {
          type: "array",
          minItems: 1,
          items: {
            type: "object",
            properties: { size: { type: "string", enum: ["s", "m"] } },
          },
        },
      },
    }
    const { errors, errorSchema } = validator.validateFormData(
      { db: { port: 70000 }, nodes: [{ size: "xl" }] },
      schema
    )
    expect(errors.map((e) => `${e.property}:${e.name}`).sort()).toEqual([
      ".db.host:required",
      ".db.port:maximum",
      ".nodes.0.size:enum",
    ])
    expect(errorSchema).toMatchObject({
      db: {
        host: { __errors: [expect.any(String)] },
        port: { __errors: [expect.any(String)] },
      },
      nodes: { 0: { size: { __errors: [expect.any(String)] } } },
    })
  })

  it("resolves $ref/$defs and definitions, including in isValid against a rootSchema", () => {
    const root: RJSFSchema = {
      $defs: { name: { type: "string", pattern: "^[a-z]+$" } },
      definitions: { count: { type: "integer", minimum: 1 } },
      type: "object",
      properties: {
        name: { $ref: "#/$defs/name", title: "Name" },
        count: { $ref: "#/definitions/count" },
      },
    }
    const { errors } = validator.validateFormData(
      { name: "A1", count: 0 },
      root
    )
    expect(errors.map((e) => `${e.property}:${e.name}`).sort()).toEqual([
      ".count:minimum",
      ".name:pattern",
    ])
    expect(byName(errors, "pattern")[0].params).toEqual({ pattern: "^[a-z]+$" })
    expect(byName(errors, "minimum")[0].params).toEqual({
      comparison: ">=",
      limit: 1,
    })

    expect(validator.isValid({ $ref: "#/$defs/name" }, "abc", root)).toBe(true)
    expect(validator.isValid({ $ref: "#/$defs/name" }, "ABC", root)).toBe(false)
    expect(
      validator.isValid(
        { $ref: "__rjsf_rootSchema#/definitions/count" },
        3,
        root
      )
    ).toBe(true)
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    expect(validator.isValid({ $ref: "#/$defs/missing" }, 1, root)).toBe(false)
    expect(warn).toHaveBeenCalled()
    warn.mockRestore()
  })

  it("matches oneOf/anyOf options and reports a single oneOf error", () => {
    const root: RJSFSchema = {
      definitions: {
        s3: {
          type: "object",
          required: ["bucket"],
          properties: { bucket: { type: "string", minLength: 3 } },
        },
        local: {
          type: "object",
          required: ["path"],
          properties: { path: { type: "string" } },
        },
      },
      type: "object",
      properties: {
        backend: {
          oneOf: [
            { $ref: "#/definitions/s3", title: "S3" },
            { $ref: "#/definitions/local", title: "Local" },
          ],
        },
        mode: { anyOf: [{ type: "string" }, { type: "integer" }] },
      },
    }
    const options = (root.properties!.backend as RJSFSchema)
      .oneOf as RJSFSchema[]
    expect(
      getClosestMatchingOption(validator, root, { path: "/tmp" }, options)
    ).toBe(1)
    expect(
      getClosestMatchingOption(validator, root, { bucket: "state" }, options)
    ).toBe(0)

    const { errors } = validator.validateFormData(
      { backend: { bucket: "x" }, mode: true },
      root,
      undefined,
      transformErrors
    )
    const oneOf = byName(errors, "oneOf")
    expect(oneOf).toHaveLength(1)
    expect(oneOf[0]).toMatchObject({
      property: ".backend",
      message: "must match exactly one of the allowed options",
    })
    expect(byName(errors, "anyOf")[0].property).toBe(".mode")
  })

  it("ignores unknown formats but validates known ones", () => {
    const schema: RJSFSchema = {
      type: "object",
      properties: {
        pw: { type: "string", format: "password", writeOnly: true },
        site: { type: "string", format: "uri" },
        odd: { type: "string", format: "x-keel-custom" },
      },
    }
    expect(
      validator.validateFormData(
        { pw: "p", odd: "?", site: "https://a.b" },
        schema
      ).errors
    ).toEqual([])
    const { errors } = validator.validateFormData({ site: "::nope" }, schema)
    expect(errors[0]).toMatchObject({
      name: "format",
      params: { format: "uri" },
    })
  })

  it("runs customValidate and merges its errors", () => {
    const schema: RJSFSchema = {
      type: "object",
      properties: { tag: { type: "string" } },
    }
    const typed = createValidator<{ tag?: string }>()
    const result = typed.validateFormData(
      { tag: "a#b" },
      schema,
      (data, errors) => {
        if (String(data?.tag).includes("#")) errors.tag?.addError("no hashes")
        return errors
      }
    )
    expect(result.errorSchema).toMatchObject({
      tag: { __errors: ["no hashes"] },
    })
    expect(result.errors.map((e) => e.stack)).toContain(".tag no hashes")
  })

  it("surfaces invalid schemas as a validationError instead of throwing", () => {
    const schema = { type: "string", pattern: "(" } as RJSFSchema
    const raw = validator.rawValidation(schema, "x")
    expect(raw.validationError).toBeInstanceOf(Error)
    const { errors, errorSchema } = validator.validateFormData(
      "x" as never,
      schema
    )
    expect(errors).toHaveLength(1)
    expect(errorSchema).toHaveProperty("$schema")
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    expect(validator.isValid(schema, "x", schema)).toBe(false)
    warn.mockRestore()
  })

  it("normalises form data into JSON instances", () => {
    expect(
      toJsonInstance({
        a: undefined,
        b: [undefined, 1],
        c: new Date(0),
        d: () => 1,
        e: BigInt(2),
      })
    ).toEqual({ b: [null, 1], c: "1970-01-01T00:00:00.000Z", e: 2 })
    expect(validator.isValid({ type: "null" }, undefined, {})).toBe(true)
    validator.reset?.()
  })
})
