import type { RJSFSchema } from "@rjsf/utils"

export type SchemaFieldType =
  "string" | "integer" | "number" | "boolean" | "object" | "array"

export interface SchemaFieldDraft {
  name: string
  title: string
  type: SchemaFieldType
  defaultValue: string
  required: boolean
  secret: boolean
  itemsType?: SchemaFieldType
  children?: SchemaFieldDraft[]
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
  if (
    raw === "integer" ||
    raw === "number" ||
    raw === "boolean" ||
    raw === "object" ||
    raw === "array"
  ) {
    return raw
  }
  return "string"
}

function formatArrayDefault(raw: unknown, itemsType: SchemaFieldType): string {
  if (!Array.isArray(raw)) return ""
  if (
    itemsType === "object" ||
    itemsType === "array" ||
    raw.some((item) => typeof item === "object" && item !== null)
  ) {
    return JSON.stringify(raw)
  }
  if (
    itemsType === "string" &&
    raw.some((item) => String(item).includes(","))
  ) {
    return JSON.stringify(raw)
  }
  return raw.map(String).join(", ")
}

function parseArrayDefault(
  raw: string,
  itemsType: SchemaFieldType
): unknown[] | undefined {
  const trimmed = raw.trim()
  if (!trimmed) return undefined
  if (trimmed.startsWith("[")) {
    try {
      const parsed = JSON.parse(trimmed)
      if (Array.isArray(parsed)) return parsed
    } catch {
      // fall through to comma-separated parsing
    }
  }
  const parts = trimmed
    .split(",")
    .map((p) => p.trim())
    .filter((p) => p.length > 0)
  if (parts.length === 0) return undefined
  if (itemsType === "integer") {
    return parts.map((p) => parseInt(p, 10)).filter((n) => !Number.isNaN(n))
  }
  if (itemsType === "number") {
    return parts.map((p) => parseFloat(p)).filter((n) => !Number.isNaN(n))
  }
  if (itemsType === "boolean") {
    return parts.map((p) => p.toLowerCase() === "true")
  }
  return parts
}

function parsePropertiesToFields(
  rawProps: unknown,
  rawRequired: unknown
): SchemaFieldDraft[] {
  if (!rawProps || typeof rawProps !== "object" || Array.isArray(rawProps)) {
    return []
  }
  const props = rawProps as Record<string, Record<string, unknown>>
  const requiredSet = new Set(
    Array.isArray(rawRequired)
      ? rawRequired.filter((x): x is string => typeof x === "string")
      : []
  )
  return Object.entries(props).map(([name, prop]) => {
    const type = normalizeSchemaType(prop?.type)
    const title =
      typeof prop?.title === "string" ? prop.title : humanizeFieldName(name)
    const required = requiredSet.has(name)

    if (type === "object") {
      return {
        name,
        title,
        type,
        defaultValue: "",
        required,
        secret: false,
        children: parsePropertiesToFields(prop?.properties, prop?.required),
      }
    }

    if (type === "array") {
      const rawItems =
        prop?.items &&
        typeof prop.items === "object" &&
        !Array.isArray(prop.items)
          ? (prop.items as Record<string, unknown>)
          : undefined
      const itemsType = normalizeSchemaType(rawItems?.type)
      const draft: SchemaFieldDraft = {
        name,
        title,
        type,
        itemsType,
        defaultValue: formatArrayDefault(prop?.default, itemsType),
        required,
        secret: false,
      }
      if (itemsType === "object") {
        draft.children = parsePropertiesToFields(
          rawItems?.properties,
          rawItems?.required
        )
      }
      return draft
    }

    const hasDefault = prop && prop.default !== undefined
    return {
      name,
      title,
      type,
      defaultValue: hasDefault ? String(prop.default) : "",
      required,
      secret: prop?.writeOnly === true || prop?.format === "password",
    }
  })
}

export function schemaJsonToFields(
  schemaText: string
): SchemaFieldDraft[] | null {
  if (!schemaText.trim()) return []
  try {
    const parsed = JSON.parse(schemaText) as Record<string, unknown>
    if (!parsed || typeof parsed !== "object") return null
    return parsePropertiesToFields(parsed.properties, parsed.required)
  } catch {
    return null
  }
}

function buildPropertiesAndRequired(
  fields: SchemaFieldDraft[],
  existingProps: Record<string, Record<string, unknown>> = {}
): {
  properties: Record<string, Record<string, unknown>>
  required: string[]
} {
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

    if (field.type === "object") {
      delete nextProp.items
      delete nextProp.default
      delete nextProp.writeOnly
      if (nextProp.format === "password") {
        delete nextProp.format
      }
      const prevNestedProps =
        prevProp.properties &&
        typeof prevProp.properties === "object" &&
        !Array.isArray(prevProp.properties)
          ? (prevProp.properties as Record<string, Record<string, unknown>>)
          : {}
      const nested = buildPropertiesAndRequired(
        field.children ?? [],
        prevNestedProps
      )
      nextProp.properties = nested.properties
      if (nested.required.length > 0) {
        nextProp.required = nested.required
      } else {
        delete nextProp.required
      }
      properties[name] = nextProp
      continue
    }

    if (field.type === "array") {
      delete nextProp.properties
      delete nextProp.required
      delete nextProp.writeOnly
      if (nextProp.format === "password") {
        delete nextProp.format
      }
      const itemsType = field.itemsType ?? "string"
      const prevItems =
        prevProp.items &&
        typeof prevProp.items === "object" &&
        !Array.isArray(prevProp.items)
          ? (prevProp.items as Record<string, unknown>)
          : {}
      if (itemsType === "object") {
        const prevItemProps =
          prevItems.properties &&
          typeof prevItems.properties === "object" &&
          !Array.isArray(prevItems.properties)
            ? (prevItems.properties as Record<string, Record<string, unknown>>)
            : {}
        const nested = buildPropertiesAndRequired(
          field.children ?? [],
          prevItemProps
        )
        const nextItems: Record<string, unknown> = {
          ...prevItems,
          type: "object",
          properties: nested.properties,
        }
        if (nested.required.length > 0) {
          nextItems.required = nested.required
        } else {
          delete nextItems.required
        }
        nextProp.items = nextItems
      } else {
        const nextItems: Record<string, unknown> =
          prevItems.type === itemsType
            ? { ...prevItems, type: itemsType }
            : { type: itemsType }
        delete nextItems.properties
        delete nextItems.required
        nextProp.items = nextItems
      }

      const parsedDefault = parseArrayDefault(field.defaultValue, itemsType)
      if (parsedDefault !== undefined) {
        nextProp.default = parsedDefault
      } else {
        delete nextProp.default
      }

      properties[name] = nextProp
      continue
    }

    delete nextProp.properties
    delete nextProp.required
    delete nextProp.items

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

  return { properties, required }
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

  const { properties, required } = buildPropertiesAndRequired(
    fields,
    existingProps
  )

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

export function extractSchemaDefaults(
  schema?: Record<string, unknown>,
  defaultValues?: Record<string, unknown>
): Record<string, unknown> {
  const result: Record<string, unknown> = { ...(defaultValues ?? {}) }
  const properties = schema?.properties
  if (
    !properties ||
    typeof properties !== "object" ||
    Array.isArray(properties)
  ) {
    return result
  }
  for (const [key, rawProp] of Object.entries(
    properties as Record<string, unknown>
  )) {
    if (!rawProp || typeof rawProp !== "object" || Array.isArray(rawProp)) {
      continue
    }
    const prop = rawProp as Record<string, unknown>
    if (result[key] === undefined && prop.default !== undefined) {
      result[key] = prop.default
    } else if (
      prop.type === "object" &&
      prop.properties &&
      typeof prop.properties === "object"
    ) {
      const nested = extractSchemaDefaults(
        prop,
        result[key] && typeof result[key] === "object"
          ? (result[key] as Record<string, unknown>)
          : undefined
      )
      if (Object.keys(nested).length > 0) {
        result[key] = nested
      }
    }
  }
  return result
}

/**
 * Scans for `${var_name}` and `${var_name:=default}` placeholders
 * and returns unique variable names in order of appearance.
 */
export function extractManifestVariables(manifestsYaml: string): string[] {
  if (!manifestsYaml) return []
  const regex = /\$\{([A-Za-z_][A-Za-z0-9_]*)(?::=[^}]*)?\}/g
  const seen = new Set<string>()
  const vars: string[] = []
  let match: RegExpExecArray | null
  while ((match = regex.exec(manifestsYaml)) !== null) {
    const varName = match[1]
    if (!seen.has(varName)) {
      seen.add(varName)
      vars.push(varName)
    }
  }
  return vars
}
