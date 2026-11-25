import { describe, it, expect } from "vitest"
import {
  HEURISTIC_SECRET_UI_OPTION,
  KEEP_EXISTING_SECRET_PLACEHOLDER,
  KEY_NAME_SUBJECTS,
  REFERENCE_SUFFIXES,
  SECRET_KEY_WORDS,
  SECRET_MASK,
  buildSecretAwareUiSchema,
  heuristicSecretCandidates,
  heuristicSecretSchemaPaths,
  looksLikeSecretKey,
  looksLikeSecretKeyUnder,
  PASS_NON_SECRETS,
  maskValuesAtPaths,
  relaxRequiredAtPaths,
  relaxSecretRequirements,
  isSecretSchemaNode,
  maskSecretValues,
  splitDotPath,
  withHeuristicSecretUi,
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

  it("keeps a secret list whole: no keep placeholder or relaxed fields inside it", () => {
    const oauth = {
      type: "object",
      properties: {
        sources: {
          type: "array",
          writeOnly: true,
          items: {
            type: "object",
            required: ["name", "secret"],
            properties: {
              name: { type: "string" },
              secret: { type: "string", format: "password" },
            },
          },
        },
      },
    }
    const ui = buildSecretAwareUiSchema(oauth, undefined, { editMode: true })
    expect(ui.sources).toEqual({
      items: { secret: { "ui:widget": "password" } },
    })
    const relaxed = relaxSecretRequirements(oauth)
    expect(relaxed.properties.sources.items.required).toEqual([
      "name",
      "secret",
    ])
  })
})

// ---------------------------------------------------------------------------------------------
// Name-based secret heuristic (AST-2): the shared corpus keeps this mirror identical to keel-core.
// ---------------------------------------------------------------------------------------------

// Node's fs is loaded through a non-literal specifier so the browser tsconfig needs no node types.
const fsModule = "node:fs"
const { readFileSync } = (await import(/* @vite-ignore */ fsModule)) as {
  readFileSync: (file: URL, encoding: "utf8") => string
}

function repoFile(relative: string): URL {
  // Bind import.meta.url first so Vite's `new URL(…, import.meta.url)` asset rewrite skips it.
  const here = import.meta.url
  return new URL(relative, here)
}

interface Corpus {
  keys: { key: string; secret: boolean; source?: string }[]
  values: {
    name: string
    schema: unknown
    values: unknown
    expected: string[]
  }[]
}

const corpus = JSON.parse(
  readFileSync(repoFile("../test/fixtures/secret-key-heuristic.json"), "utf8")
) as Corpus

describe("name-based secret heuristic (shared corpus with keel-core)", () => {
  it("never matches keys of annotation and label maps", () => {
    expect(looksLikeSecretKeyUnder(undefined, "apiToken")).toBe(true)
    expect(looksLikeSecretKeyUnder("auth", "apiToken")).toBe(true)
    for (const parent of ["annotations", "podAnnotations", "commonLabels"]) {
      expect(looksLikeSecretKeyUnder(parent, "apiToken")).toBe(false)
    }
  })

  it("classifies every corpus key like the server", () => {
    expect(corpus.keys.length).toBeGreaterThan(50)
    const failures = corpus.keys
      .filter((c) => looksLikeSecretKey(c.key) !== c.secret)
      .map((c) => `${c.key}: expected secret=${c.secret}`)
    expect(failures).toEqual([])
  })

  it.each(corpus.values.map((c) => [c.name, c] as const))(
    "flags the same values as the server: %s",
    (_name, c) => {
      expect(
        heuristicSecretCandidates(c.schema, c.values).map((l) => l.path)
      ).toEqual(c.expected)
    }
  )

  it("uses the same word lists as keel-core", () => {
    const rust = readFileSync(
      repoFile("../../../crates/keel-core/src/secret_heuristic.rs"),
      "utf8"
    )
    const list = (name: string): string[] => {
      const match = new RegExp(
        `${name}: &\\[&str\\] = &\\[([\\s\\S]*?)\\];`
      ).exec(rust)
      expect(match, name).not.toBeNull()
      return [...match![1].matchAll(/"([^"]+)"/g)].map((m) => m[1])
    }
    expect([...SECRET_KEY_WORDS]).toEqual(list("SECRET_KEY_WORDS"))
    expect([...REFERENCE_SUFFIXES]).toEqual(list("REFERENCE_SUFFIXES"))
    expect([...KEY_NAME_SUBJECTS]).toEqual(list("KEY_NAME_SUBJECTS"))
    expect([...PASS_NON_SECRETS]).toEqual(list("PASS_NON_SECRETS"))
  })

  it("follows $ref, patternProperties and combinators like the server resolver", () => {
    const resolverSchema = {
      $defs: { plain: { type: "string", writeOnly: false } },
      properties: {
        tlsSecret: { $ref: "#/$defs/plain" },
        opts: {
          patternProperties: { "^.*Token$": { format: "password" } },
        },
      },
    }
    expect(
      heuristicSecretCandidates(resolverSchema, {
        tlsSecret: "my-tls",
        opts: { apiToken: "a", webhookSecret: "w" },
      })
    ).toEqual([{ pointer: "/opts/webhookSecret", path: "opts.webhookSecret" }])
  })

  it("escapes JSON pointer tokens and keeps list paths", () => {
    expect(
      heuristicSecretCandidates(
        { type: "object" },
        { "a~b": { token: "t" }, users: [{ password: "p" }] }
      )
    ).toEqual([
      { pointer: "/a~0b/token", path: "a~b.token" },
      { pointer: "/users/0/password", path: "users[].password" },
    ])
  })
})

describe("heuristicSecretSchemaPaths", () => {
  it("lists schema-named fields the heuristic will treat as secret", () => {
    const formSchema = {
      type: "object",
      properties: {
        auth: {
          type: "object",
          properties: {
            password: { type: "string" },
            existingSecret: { type: "string" },
            adminPassword: { type: "string", writeOnly: true },
            tokenMode: { type: "string" },
          },
        },
        apiTokens: { type: "array", items: { type: "string" } },
        secretCount: { type: "integer" },
        tlsSecret: { type: "string", writeOnly: false },
        backendKind: { type: "string" },
        // An enum is no opt-out (only writeOnly: false is).
        token: { type: "string", enum: ["a", "b"] },
        pin: { type: "integer" },
        enabled: { type: "boolean" },
        users: {
          type: "array",
          items: {
            type: "object",
            properties: { password: { type: "string" } },
          },
        },
        smtp: {
          type: "object",
          writeOnly: true,
          properties: { password: { type: "string" } },
        },
        ref: { $ref: "#/$defs/creds" },
      },
      $defs: {
        creds: { type: "object", properties: { clientSecret: {} } },
      },
    }
    expect(heuristicSecretSchemaPaths(formSchema)).toEqual([
      "apiTokens",
      "auth.password",
      "ref.clientSecret",
      "token",
      "users[].password",
    ])
  })

  it("terminates on recursive $ref schemas", () => {
    const recursive = {
      $defs: {
        node: {
          type: "object",
          properties: { child: { $ref: "#/$defs/node" }, token: {} },
        },
      },
      $ref: "#/$defs/node",
    }
    // Each schema node is walked once: the cycle stops after the first repetition.
    expect(heuristicSecretSchemaPaths(recursive)).toEqual([
      "child.token",
      "token",
    ])
  })
})

describe("heuristic presentation helpers", () => {
  it("masks values at dot paths, including list items", () => {
    expect(
      maskValuesAtPaths(
        {
          auth: { password: "hunter2", user: "u" },
          users: [{ password: "p" }, { name: "n" }],
          empty: null,
        },
        ["auth.password", "users[].password", "missing.path", "empty"]
      )
    ).toEqual({
      auth: { password: SECRET_MASK, user: "u" },
      users: [{ password: SECRET_MASK }, { name: "n" }],
      empty: null,
    })
  })

  it("splits dot paths with list segments", () => {
    expect(splitDotPath("a.users[].b[][]")).toEqual([
      "a",
      "users",
      "[]",
      "b",
      "[]",
      "[]",
    ])
  })

  it("adds password widgets, placeholders and the notice option", () => {
    const ui = withHeuristicSecretUi(
      { auth: { password: { "ui:help": "h" } } },
      [
        { path: "auth.password", notSecret: false, fromSchema: true },
        { path: "users[].token", notSecret: true, fromSchema: true },
        { path: "extra.apiKey", notSecret: false, fromSchema: false },
      ],
      { editMode: true }
    )
    expect(ui).toEqual({
      auth: {
        password: {
          "ui:help": "h",
          "ui:widget": "password",
          "ui:placeholder": KEEP_EXISTING_SECRET_PLACEHOLDER,
          "ui:options": {
            [HEURISTIC_SECRET_UI_OPTION]: {
              path: "auth.password",
              notSecret: false,
              fromSchema: true,
            },
          },
        },
      },
      users: {
        items: {
          token: {
            "ui:options": {
              [HEURISTIC_SECRET_UI_OPTION]: {
                path: "users[].token",
                notSecret: true,
                fromSchema: true,
              },
            },
          },
        },
      },
      extra: {
        apiKey: {
          "ui:options": {
            [HEURISTIC_SECRET_UI_OPTION]: {
              path: "extra.apiKey",
              notSecret: false,
              fromSchema: false,
            },
          },
        },
      },
    })
  })

  it("relaxes required only at the given object paths", () => {
    const required = {
      type: "object",
      required: ["token", "name"],
      properties: {
        token: { type: "string" },
        name: { type: "string" },
        auth: {
          type: "object",
          required: ["password"],
          properties: { password: { type: "string" } },
        },
      },
    }
    expect(
      relaxRequiredAtPaths(required, ["token", "auth.password", "list[].x"])
    ).toEqual({
      ...required,
      required: ["name"],
      properties: {
        ...required.properties,
        auth: { ...required.properties.auth, required: [] },
      },
    })
  })

  it("masks secrets reached through $ref and patternProperties in previews", () => {
    expect(
      maskSecretValues(
        {
          $defs: { pw: { type: "string", format: "password" } },
          properties: {
            db: { properties: { pass: { $ref: "#/$defs/pw" } } },
            env: { patternProperties: { _KEY$: { writeOnly: true } } },
          },
        },
        { db: { pass: "x" }, env: { API_KEY: "k", HOST: "h" } }
      )
    ).toEqual({
      db: { pass: SECRET_MASK },
      env: { API_KEY: SECRET_MASK, HOST: "h" },
    })
  })
})
