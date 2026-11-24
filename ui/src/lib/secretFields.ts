/**
 * Helpers for secret (`writeOnly` / `format: password`) catalog schema fields so the UI never
 * echoes secret material in previews and always renders secrets with password widgets.
 */

export const SECRET_MASK = "********"

type SchemaNode = Record<string, unknown>
type UiSchema = Record<string, unknown>

function asNode(value: unknown): SchemaNode | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as SchemaNode)
    : undefined
}

/** Mirrors `keel_core::is_secret_schema_node`: `writeOnly: true` or `format: password`. */
export function isSecretSchemaNode(node: unknown): boolean {
  const n = asNode(node)
  return Boolean(n && (n.writeOnly === true || n.format === "password"))
}

/** Collects the schema nodes that may describe `key` (direct + allOf/anyOf/oneOf branches). */
function propertyCandidates(schema: SchemaNode, key: string): SchemaNode[] {
  const out: SchemaNode[] = []
  const direct = asNode(asNode(schema.properties)?.[key])
  if (direct) out.push(direct)
  for (const comb of ["allOf", "anyOf", "oneOf"] as const) {
    const branches = schema[comb]
    if (Array.isArray(branches)) {
      for (const b of branches) {
        const node = asNode(b)
        if (node) out.push(...propertyCandidates(node, key))
      }
    }
  }
  if (out.length === 0) {
    const additional = asNode(schema.additionalProperties)
    if (additional) out.push(additional)
  }
  return out
}

function maskNode(schemaNodes: SchemaNode[], value: unknown): unknown {
  if (value === undefined || value === null) return value
  if (schemaNodes.some(isSecretSchemaNode)) return SECRET_MASK
  if (Array.isArray(value)) {
    const itemSchemas = schemaNodes
      .map((n) => asNode(n.items))
      .filter((n): n is SchemaNode => Boolean(n))
    return value.map((item) => maskNode(itemSchemas, item))
  }
  if (typeof value === "object") {
    const out: Record<string, unknown> = {}
    for (const [key, child] of Object.entries(value as SchemaNode)) {
      const childSchemas = schemaNodes.flatMap((n) =>
        propertyCandidates(n, key)
      )
      out[key] = maskNode(childSchemas, child)
    }
    return out
  }
  return value
}

/**
 * Returns a copy of `data` where every value whose schema node is secret is replaced by
 * {@link SECRET_MASK}. Used for display-only previews; never for submitted payloads.
 */
export function maskSecretValues<T>(schema: unknown, data: T): T {
  const root = asNode(schema)
  return maskNode(root ? [root] : [], data) as T
}

export const KEEP_EXISTING_SECRET_PLACEHOLDER =
  "Leave blank to keep existing secret"

function secretUiNode(
  schema: SchemaNode,
  ui: UiSchema,
  editMode: boolean
): UiSchema {
  const out: UiSchema = { ...ui }
  const types = Array.isArray(schema.type) ? schema.type : [schema.type]
  const secret = isSecretSchemaNode(schema)
  if (secret && (types.includes("string") || schema.type === undefined)) {
    if (out["ui:widget"] === undefined) out["ui:widget"] = "password"
    if (editMode) out["ui:placeholder"] = KEEP_EXISTING_SECRET_PLACEHOLDER
  }
  // A secret object/list is stored (and kept) as a whole: a blank field inside it keeps
  // nothing, so its nested secrets get password widgets but no "keep" placeholder.
  if (secret) editMode = false
  const props = asNode(schema.properties)
  if (props) {
    for (const [key, child] of Object.entries(props)) {
      const childNode = asNode(child)
      if (!childNode) continue
      const merged = secretUiNode(childNode, asNode(out[key]) ?? {}, editMode)
      if (Object.keys(merged).length > 0) out[key] = merged
    }
  }
  const items = asNode(schema.items)
  if (items) {
    const merged = secretUiNode(items, asNode(out.items) ?? {}, editMode)
    if (Object.keys(merged).length > 0) out.items = merged
  }
  return out
}

/**
 * Merges the catalog-provided `uiSchema` with `ui:widget: password` for every secret string
 * field (including `writeOnly` fields without `format: password`). Explicit catalog widgets win.
 */
export function buildSecretAwareUiSchema(
  schema: unknown,
  uiSchema?: Record<string, unknown>,
  options: { editMode?: boolean } = {}
): UiSchema {
  const root = asNode(schema)
  if (!root) return { ...(uiSchema ?? {}) }
  return secretUiNode(root, uiSchema ?? {}, Boolean(options.editMode))
}

/**
 * Edit mode: existing secret values are never sent back to the browser, so secret fields
 * must not be `required` (blank keeps the stored secret). Returns a copy of `schema` with
 * every secret property removed from its parent's `required` list (recursively). The inside of
 * a secret object/list is left as is: it is stored and replaced as a whole, so a re-entered
 * one must be complete (the server validates it the same way).
 */
export function relaxSecretRequirements<T>(schema: T): T {
  const node = asNode(schema)
  if (!node || isSecretSchemaNode(node)) return schema
  const out: SchemaNode = { ...node }
  const props = asNode(node.properties)
  if (props) {
    const nextProps: SchemaNode = {}
    for (const [key, child] of Object.entries(props)) {
      nextProps[key] = relaxSecretRequirements(child)
    }
    out.properties = nextProps
    if (Array.isArray(node.required)) {
      out.required = (node.required as string[]).filter(
        (k) => !isSecretSchemaNode(props[k])
      )
    }
  }
  if (asNode(node.items)) out.items = relaxSecretRequirements(node.items)
  for (const comb of ["allOf", "anyOf", "oneOf"] as const) {
    if (Array.isArray(node[comb])) {
      out[comb] = (node[comb] as unknown[]).map((b) =>
        relaxSecretRequirements(b)
      )
    }
  }
  return out as T
}
