import type { CatalogBundleItem } from "../types/api"

/**
 * Values key of the hostname a Helm app's Flagger canary serves on the shared `keel-ingress`
 * Gateway (mirrors `CANARY_HOST_VALUE` in keel-server). A canary without one would make Flagger
 * write catch-all routes that capture other tenants' traffic, so the server rejects it (`400`).
 */
export const CANARY_HOST_VALUE = "ingressHost"

type SchemaNode = Record<string, unknown>

const asNode = (value: unknown): SchemaNode | null =>
  value && typeof value === "object" && !Array.isArray(value)
    ? (value as SchemaNode)
    : null

/** Field added when a canary-capable app's schema has no `ingressHost` property. */
const CANARY_HOST_FIELD: SchemaNode = {
  type: "string",
  title: "Canary Hostname",
  description:
    "DNS hostname the Flagger canary serves through the shared Envoy Gateway (required for canaries).",
  format: "hostname",
}

/**
 * Returns a copy of an app values schema that requires a non-empty `ingressHost` (added as a
 * **Canary Hostname** field when the schema has none). Unchanged when `enabled` is false.
 */
export function withCanaryHostRequired<T>(schema: T, enabled: boolean): T {
  if (!enabled) return schema
  const node = asNode(schema) ?? { type: "object" }
  const props = { ...(asNode(node.properties) ?? {}) }
  const existing = asNode(props[CANARY_HOST_VALUE])
  props[CANARY_HOST_VALUE] = existing
    ? { minLength: 1, ...existing }
    : CANARY_HOST_FIELD
  const required = Array.isArray(node.required)
    ? (node.required as string[])
    : []
  return {
    ...node,
    properties: props,
    required: required.includes(CANARY_HOST_VALUE)
      ? required
      : [...required, CANARY_HOST_VALUE],
  } as T
}

/**
 * Bundle variant: every `canary: true` app item's sub-schema (`properties.<item id>`) requires
 * its `ingressHost` (see {@link withCanaryHostRequired}); the item object itself becomes required.
 */
export function withBundleCanaryHostsRequired<T>(
  schema: T,
  items: CatalogBundleItem[] | undefined
): T {
  const canaryItems = (items ?? []).filter(
    (item) => item.kind === "app" && item.canary
  )
  if (canaryItems.length === 0) return schema
  const node = asNode(schema) ?? { type: "object" }
  const props = { ...(asNode(node.properties) ?? {}) }
  const required = Array.isArray(node.required)
    ? [...(node.required as string[])]
    : []
  for (const item of canaryItems) {
    props[item.id] = withCanaryHostRequired(
      asNode(props[item.id]) ?? { type: "object" },
      true
    )
    if (!required.includes(item.id)) required.push(item.id)
  }
  return { ...node, properties: props, required } as T
}
