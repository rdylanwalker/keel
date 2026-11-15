import type { RJSFSchema } from "@rjsf/utils"

export type SchemaFieldType = "string" | "integer" | "number" | "boolean"

export interface SchemaFieldDraft {
  name: string
  title: string
  type: SchemaFieldType
  defaultValue: string
  required: boolean
  secret: boolean
}

export const PROVIDER_CREDENTIAL_PRESETS: Record<string, SchemaFieldDraft[]> = {
  aws: [
    {
      name: "AWS_ACCESS_KEY_ID",
      title: "AWS Access Key ID",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
    {
      name: "AWS_SECRET_ACCESS_KEY",
      title: "AWS Secret Access Key",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
  ],
  gcp: [
    {
      name: "GOOGLE_CREDENTIALS",
      title: "GCP Service Account JSON Key",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
  ],
  azure: [
    {
      name: "ARM_CLIENT_ID",
      title: "Azure Service Principal Client ID",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
    {
      name: "ARM_CLIENT_SECRET",
      title: "Azure Service Principal Client Secret",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
    {
      name: "ARM_TENANT_ID",
      title: "Azure Tenant ID",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
    {
      name: "ARM_SUBSCRIPTION_ID",
      title: "Azure Subscription ID",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
  ],
  proxmox: [
    {
      name: "PROXMOX_VE_ENDPOINT",
      title: "Proxmox VE API Endpoint",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
    {
      name: "PROXMOX_VE_API_TOKEN",
      title: "Proxmox VE API Token",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
  ],
  hetzner: [
    {
      name: "HCLOUD_TOKEN",
      title: "Hetzner Cloud API Token",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
  ],
  cloudflare: [
    {
      name: "CLOUDFLARE_API_TOKEN",
      title: "Cloudflare API Token",
      type: "string",
      defaultValue: "",
      required: true,
      secret: true,
    },
  ],
}

export function humanizeFieldName(name: string): string {
  return name
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/[_-]+/g, " ")
    .replace(/\b\w/g, (c) => c.toUpperCase())
    .trim()
}

function normalizeSchemaType(raw: unknown): SchemaFieldType {
  if (raw === "integer" || raw === "number" || raw === "boolean") {
    return raw
  }
  return "string"
}

export function schemaJsonToFields(
  schemaText: string
): SchemaFieldDraft[] | null {
  if (!schemaText.trim()) return []
  try {
    const parsed = JSON.parse(schemaText) as Record<string, unknown>
    if (!parsed || typeof parsed !== "object") return null
    const props = (parsed.properties ?? {}) as Record<
      string,
      Record<string, unknown>
    >
    const requiredSet = new Set(
      Array.isArray(parsed.required)
        ? parsed.required.filter((x): x is string => typeof x === "string")
        : []
    )
    return Object.entries(props).map(([name, prop]) => {
      const type = normalizeSchemaType(prop?.type)
      const hasDefault = prop && prop.default !== undefined
      return {
        name,
        title:
          typeof prop?.title === "string"
            ? prop.title
            : humanizeFieldName(name),
        type,
        defaultValue: hasDefault ? String(prop.default) : "",
        required: requiredSet.has(name),
        secret: prop?.writeOnly === true || prop?.format === "password",
      }
    })
  } catch {
    return null
  }
}

export function fieldsToSchemaJson(
  fields: SchemaFieldDraft[],
  existingSchemaText?: string
): string {
  let base: Record<string, unknown> = { type: "object" }
  let existingProps: Record<string, Record<string, unknown>> = {}
  if (existingSchemaText?.trim()) {
    try {
      const parsed = JSON.parse(existingSchemaText) as Record<string, unknown>
      if (parsed && typeof parsed === "object") {
        base = { ...parsed, type: "object" }
        if (parsed.properties && typeof parsed.properties === "object") {
          existingProps = parsed.properties as Record<
            string,
            Record<string, unknown>
          >
        }
      }
    } catch {
      // ignore invalid existing JSON
    }
  }

  const required: string[] = []
  const properties: Record<string, Record<string, unknown>> = {}

  for (const field of fields) {
    const name = field.name.trim()
    if (!name) continue
    if (field.required) required.push(name)

    const prevProp = existingProps[name] ?? {}
    const nextProp: Record<string, unknown> = {
      ...prevProp,
      type: field.type,
      title: field.title.trim() || humanizeFieldName(name),
    }

    if (field.defaultValue !== "") {
      if (field.type === "integer") {
        const n = parseInt(field.defaultValue, 10)
        if (!Number.isNaN(n)) nextProp.default = n
      } else if (field.type === "number") {
        const n = parseFloat(field.defaultValue)
        if (!Number.isNaN(n)) nextProp.default = n
      } else if (field.type === "boolean") {
        nextProp.default = field.defaultValue.toLowerCase() === "true"
      } else {
        nextProp.default = field.defaultValue
      }
    } else {
      delete nextProp.default
    }

    if (field.secret) {
      nextProp.format = "password"
      nextProp.writeOnly = true
    } else {
      delete nextProp.writeOnly
      if (nextProp.format === "password") {
        delete nextProp.format
      }
    }

    properties[name] = nextProp
  }

  const result: RJSFSchema & Record<string, unknown> = {
    ...base,
    type: "object",
    ...(required.length > 0 ? { required } : {}),
    properties: properties as RJSFSchema["properties"],
  }
  if (required.length === 0) {
    delete result.required
  }

  return JSON.stringify(result, null, 2)
}

function unquoteYamlScalar(raw: string): string {
  const trimmed = raw.trim()
  if (
    (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
    (trimmed.startsWith("'") && trimmed.endsWith("'"))
  ) {
    return trimmed.slice(1, -1)
  }
  return trimmed
}

/**
 * Extracts top-level `config:` (or `configuration:`) entries from a `Pulumi.yaml` program
 * into `SchemaFieldDraft[]`.
 */
export function deriveFieldsFromPulumiYaml(
  pulumiYaml: string
): SchemaFieldDraft[] {
  const lines = pulumiYaml.split(/\r?\n/)
  let inConfig = false
  let entryIndent: number | null = null
  let currentKey: string | null = null
  const entries: Array<{
    name: string
    type: SchemaFieldType
    defaultValue: string
    hasDefault: boolean
  }> = []

  const pushCurrent = (
    name: string | null,
    buf: { type: SchemaFieldType; defaultValue: string; hasDefault: boolean }
  ) => {
    if (!name) return
    entries.push({
      name,
      type: buf.type,
      defaultValue: buf.defaultValue,
      hasDefault: buf.hasDefault,
    })
  }

  let currentProps = {
    type: "string" as SchemaFieldType,
    defaultValue: "",
    hasDefault: false,
  }

  for (const rawLine of lines) {
    const line = rawLine.replace(/\t/g, "  ")
    const stripped = line.trim()
    if (!stripped || stripped.startsWith("#")) continue

    const indent = line.length - line.trimStart().length

    if (indent === 0) {
      if (inConfig) {
        pushCurrent(currentKey, currentProps)
        currentKey = null
        inConfig = false
      }
      if (stripped === "config:" || stripped === "configuration:") {
        inConfig = true
        entryIndent = null
      }
      continue
    }

    if (!inConfig) continue

    if (entryIndent === null) {
      entryIndent = indent
    }

    if (indent === entryIndent && stripped.endsWith(":")) {
      pushCurrent(currentKey, currentProps)
      currentKey = stripped.slice(0, -1).trim()
      currentProps = {
        type: "string",
        defaultValue: "",
        hasDefault: false,
      }
      continue
    }

    if (currentKey && indent > entryIndent) {
      const colonIdx = stripped.indexOf(":")
      if (colonIdx !== -1) {
        const k = stripped.slice(0, colonIdx).trim()
        const v = unquoteYamlScalar(stripped.slice(colonIdx + 1))
        if (k === "type") {
          currentProps.type = normalizeSchemaType(v)
        } else if (k === "default" && v !== "") {
          currentProps.defaultValue = v
          currentProps.hasDefault = true
        }
      }
    }
  }

  if (inConfig) {
    pushCurrent(currentKey, currentProps)
  }

  return entries.map((e) => ({
    name: e.name,
    title: humanizeFieldName(e.name),
    type: e.type,
    defaultValue: e.defaultValue,
    required: !e.hasDefault,
    secret: false,
  }))
}
