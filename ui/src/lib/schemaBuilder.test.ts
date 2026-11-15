import { describe, it, expect } from "vitest"
import {
  PROVIDER_CREDENTIAL_PRESETS,
  deriveFieldsFromPulumiYaml,
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
})
