import { describe, it, expect } from "vitest"
import {
  PROVIDER_CREDENTIAL_PRESETS,
  deriveFieldsFromPulumiYaml,
  extractManifestVariables,
  fieldsToSchemaJson,
  humanizeFieldName,
  schemaJsonToFields,
} from "./schemaBuilder"

describe("schemaBuilder", () => {
  it("humanizes camelCase and snake_case field names", () => {
    expect(humanizeFieldName("clusterName")).toBe("Cluster Name")
    expect(humanizeFieldName("worker_count")).toBe("Worker Count")
    expect(humanizeFieldName("vm-id")).toBe("Vm Id")
  })

  it("derives fields from Pulumi.yaml config block", () => {
    const pulumiYaml = `
name: gcp-free-vm
runtime: yaml
config:
  vmName:
    type: string
    default: keel-free-vm
  project:
    type: string
  workerCount:
    type: integer
    default: 2
  enablePublicIp:
    type: boolean
    default: false
resources:
  freeVm:
    type: gcp:compute:Instance
`
    const fields = deriveFieldsFromPulumiYaml(pulumiYaml)
    expect(fields).toEqual([
      {
        name: "vmName",
        title: "Vm Name",
        type: "string",
        defaultValue: "keel-free-vm",
        required: false,
        secret: false,
      },
      {
        name: "project",
        title: "Project",
        type: "string",
        defaultValue: "",
        required: true,
        secret: false,
      },
      {
        name: "workerCount",
        title: "Worker Count",
        type: "integer",
        defaultValue: "2",
        required: false,
        secret: false,
      },
      {
        name: "enablePublicIp",
        title: "Enable Public Ip",
        type: "boolean",
        defaultValue: "false",
        required: false,
        secret: false,
      },
    ])
  })

  it("round-trips fields to JSON Schema and back, preserving secrets and types", () => {
    const fields = [
      {
        name: "vmName",
        title: "VM Name",
        type: "string" as const,
        defaultValue: "keel-free-vm",
        required: true,
        secret: false,
      },
      {
        name: "count",
        title: "Count",
        type: "integer" as const,
        defaultValue: "3",
        required: false,
        secret: false,
      },
      ...PROVIDER_CREDENTIAL_PRESETS.gcp,
    ]

    const json = fieldsToSchemaJson(fields)
    const parsed = JSON.parse(json)
    expect(parsed.required).toEqual(["vmName", "GOOGLE_CREDENTIALS"])
    expect(parsed.properties.vmName.default).toBe("keel-free-vm")
    expect(parsed.properties.count.default).toBe(3)
    expect(parsed.properties.GOOGLE_CREDENTIALS.writeOnly).toBe(true)
    expect(parsed.properties.GOOGLE_CREDENTIALS.format).toBe("password")

    const back = schemaJsonToFields(json)
    expect(back).toEqual(fields)
  })

  it("returns null for invalid JSON schema text", () => {
    expect(schemaJsonToFields("{not valid json")).toBeNull()
  })

  it("handles number, boolean, non-secret password cleanup, and quoted YAML scalars", () => {
    const existing = JSON.stringify({
      type: "object",
      properties: {
        token: { type: "string", format: "password", writeOnly: true },
      },
    })
    const json = fieldsToSchemaJson(
      [
        {
          name: "ratio",
          title: "",
          type: "number",
          defaultValue: "1.5",
          required: false,
          secret: false,
        },
        {
          name: "enabled",
          title: "Enabled",
          type: "boolean",
          defaultValue: "true",
          required: false,
          secret: false,
        },
        {
          name: "token",
          title: "Token",
          type: "string",
          defaultValue: "",
          required: false,
          secret: false,
        },
      ],
      existing
    )
    const parsed = JSON.parse(json)
    expect(parsed.required).toBeUndefined()
    expect(parsed.properties.ratio.default).toBe(1.5)
    expect(parsed.properties.enabled.default).toBe(true)
    expect(parsed.properties.token.format).toBeUndefined()
    expect(parsed.properties.token.writeOnly).toBeUndefined()

    const quotedYaml = `
config:
  region:
    type: string
    default: 'us-east-1'
`
    expect(deriveFieldsFromPulumiYaml(quotedYaml)[0].defaultValue).toBe(
      "us-east-1"
    )
  })

  it("round-trips nested object fields and preserves nested required/secrets", () => {
    const fields = [
      {
        name: "database",
        title: "Database",
        type: "object" as const,
        defaultValue: "",
        required: true,
        secret: false,
        children: [
          {
            name: "host",
            title: "Host",
            type: "string" as const,
            defaultValue: "localhost",
            required: true,
            secret: false,
          },
          {
            name: "password",
            title: "Password",
            type: "string" as const,
            defaultValue: "",
            required: true,
            secret: true,
          },
        ],
      },
    ]

    const json = fieldsToSchemaJson(fields)
    const parsed = JSON.parse(json)
    expect(parsed.required).toEqual(["database"])
    expect(parsed.properties.database.type).toBe("object")
    expect(parsed.properties.database.required).toEqual(["host", "password"])
    expect(parsed.properties.database.properties.host.default).toBe("localhost")
    expect(parsed.properties.database.properties.password.writeOnly).toBe(true)

    expect(schemaJsonToFields(json)).toEqual(fields)
  })

  it("round-trips array fields of primitives and objects", () => {
    const fields = [
      {
        name: "hosts",
        title: "Allowed Hosts",
        type: "array" as const,
        itemsType: "string" as const,
        defaultValue: "a.example.com, b.example.com",
        required: true,
        secret: false,
      },
      {
        name: "ports",
        title: "Ports",
        type: "array" as const,
        itemsType: "integer" as const,
        defaultValue: "80, 443",
        required: false,
        secret: false,
      },
      {
        name: "routes",
        title: "Routes",
        type: "array" as const,
        itemsType: "object" as const,
        defaultValue: "",
        required: false,
        secret: false,
        children: [
          {
            name: "path",
            title: "Path",
            type: "string" as const,
            defaultValue: "/",
            required: true,
            secret: false,
          },
          {
            name: "port",
            title: "Port",
            type: "integer" as const,
            defaultValue: "8080",
            required: false,
            secret: false,
          },
        ],
      },
    ]

    const json = fieldsToSchemaJson(fields)
    const parsed = JSON.parse(json)
    expect(parsed.required).toEqual(["hosts"])
    expect(parsed.properties.hosts.type).toBe("array")
    expect(parsed.properties.hosts.items).toEqual({ type: "string" })
    expect(parsed.properties.hosts.default).toEqual([
      "a.example.com",
      "b.example.com",
    ])
    expect(parsed.properties.ports.items).toEqual({ type: "integer" })
    expect(parsed.properties.ports.default).toEqual([80, 443])
    expect(parsed.properties.routes.items.type).toBe("object")
    expect(parsed.properties.routes.items.required).toEqual(["path"])
    expect(parsed.properties.routes.items.properties.path.default).toBe("/")
    expect(parsed.properties.routes.items.properties.port.default).toBe(8080)

    expect(schemaJsonToFields(json)).toEqual(fields)
  })

  it("cleans up stale object/array schema keys when switching field types", () => {
    const existing = JSON.stringify({
      type: "object",
      properties: {
        config: {
          type: "object",
          required: ["a"],
          properties: { a: { type: "string" } },
        },
        tags: {
          type: "array",
          items: { type: "string" },
          default: ["x"],
        },
      },
    })

    const json = fieldsToSchemaJson(
      [
        {
          name: "config",
          title: "Config",
          type: "string",
          defaultValue: "plain",
          required: false,
          secret: false,
        },
        {
          name: "tags",
          title: "Tags",
          type: "object",
          defaultValue: "",
          required: false,
          secret: false,
          children: [],
        },
      ],
      existing
    )
    const parsed = JSON.parse(json)
    expect(parsed.properties.config).toEqual({
      type: "string",
      title: "Config",
      default: "plain",
    })
    expect(parsed.properties.tags).toEqual({
      type: "object",
      title: "Tags",
      properties: {},
    })
  })

  it("extracts unique manifest variables in order with or without defaults", () => {
    const yaml = `
apiVersion: apps/v1
kind: Deployment
metadata:
  name: \${appName}
spec:
  replicas: \${replicas:=2}
  template:
    spec:
      containers:
        - name: app
          image: \${image:=nginx:alpine}
          env:
            - name: API_KEY
              value: \${api_key}
            - name: DUPLICATE_APP
              value: \${appName}
            - name: DB_PASSWORD
              value: \${db_password:=secret}
`
    const vars = extractManifestVariables(yaml)
    expect(vars).toEqual([
      "appName",
      "replicas",
      "image",
      "api_key",
      "db_password",
    ])
  })

  it("returns empty array when manifestsYaml is empty or has no variables", () => {
    expect(extractManifestVariables("")).toEqual([])
    expect(
      extractManifestVariables("kind: Namespace\nmetadata:\n  name: production")
    ).toEqual([])
  })
})
