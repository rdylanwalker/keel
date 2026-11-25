/**
 * Local-`$ref`-aware view of a catalog JSON Schema: the UI mirror of
 * `keel_core::SchemaResolver` (`crates/keel-core/src/secret_schema.rs`).
 *
 * A "schema set" is every schema node that applies at one instance location. A location is
 * secret when any of them (after expansion through `$ref`, `allOf`/`anyOf`/`oneOf`,
 * `if`/`then`/`else` and `dependentSchemas`/`dependencies`) carries a secret marker
 * (`writeOnly: true` or `format: "password"`).
 */

export type SchemaNode = Record<string, unknown>

const COMBINATORS = ["allOf", "anyOf", "oneOf"] as const
const CONDITIONALS = ["if", "then", "else"] as const
/** Keywords holding instance data, never schemas (skipped when looking for `$anchor`). */
const DATA_KEYWORDS = new Set([
  "const",
  "enum",
  "default",
  "examples",
  "example",
])

export function asSchemaNode(value: unknown): SchemaNode | undefined {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as SchemaNode)
    : undefined
}

/** Whether `node` itself (no `$ref` or combinator resolution) carries a secret marker. */
export function hasSecretMarker(node: unknown): boolean {
  const n = asSchemaNode(node)
  return Boolean(n && (n.writeOnly === true || n.format === "password"))
}

/** Splits a dot path into segments, with each `[]` as its own segment. */
export function splitDotPath(path: string): string[] {
  const out: string[] = []
  for (const segment of path.split(".").filter(Boolean)) {
    const bracket = segment.indexOf("[")
    const name = bracket < 0 ? segment : segment.slice(0, bracket)
    if (name) out.push(name)
    let brackets = bracket < 0 ? "" : segment.slice(bracket)
    while (brackets.startsWith("[]")) {
      out.push("[]")
      brackets = brackets.slice(2)
    }
  }
  return out
}

function percentDecode(s: string): string | undefined {
  try {
    return decodeURIComponent(s)
  } catch {
    return undefined
  }
}

function findAnchor(node: unknown, anchor: string): SchemaNode | undefined {
  if (Array.isArray(node)) {
    for (const item of node) {
      const found = findAnchor(item, anchor)
      if (found) return found
    }
    return undefined
  }
  const obj = asSchemaNode(node)
  if (!obj) return undefined
  if (obj.$anchor === anchor) return obj
  for (const [key, value] of Object.entries(obj)) {
    if (DATA_KEYWORDS.has(key)) continue
    const found = findAnchor(value, anchor)
    if (found) return found
  }
  return undefined
}

function resolvePointer(root: unknown, pointer: string): unknown {
  let node: unknown = root
  for (const raw of pointer.split("/").slice(1)) {
    const token = raw.replace(/~1/g, "/").replace(/~0/g, "~")
    if (Array.isArray(node)) {
      if (!/^\d+$/.test(token)) return undefined
      node = node[Number(token)]
    } else {
      const obj = asSchemaNode(node)
      if (!obj || !Object.prototype.hasOwnProperty.call(obj, token)) {
        return undefined
      }
      node = obj[token]
    }
  }
  return node
}

/**
 * Whether the `patternProperties` regex `pattern` matches `key`. A pattern that does not compile
 * counts as a match: for secrets that over-approximates (like the server).
 */
function patternMatches(pattern: string, key: string): boolean {
  try {
    return new RegExp(pattern, "u").test(key)
  } catch {
    return true
  }
}

export class SchemaResolver {
  constructor(readonly root: unknown) {}

  private rootId(): string | undefined {
    const root = asSchemaNode(this.root)
    const id = root?.$id ?? root?.id
    if (typeof id !== "string") return undefined
    const trimmed = id.replace(/#+$/, "")
    return trimmed || undefined
  }

  private localFragment(reference: string): string | undefined {
    if (reference.startsWith("#")) return reference
    const id = this.rootId()
    if (!id || !reference.startsWith(id)) return undefined
    const rest = reference.slice(id.length)
    if (rest === "") return "#"
    return rest.startsWith("#") ? rest : undefined
  }

  /** Resolves a local `$ref` (`#`, `#/pointer`, `#anchor`, or the root `$id` plus one of those). */
  resolveRef(reference: string): unknown {
    const fragment = this.localFragment(reference)
    if (fragment === undefined) return undefined
    const decoded = percentDecode(fragment.slice(1))
    if (decoded === undefined) return undefined
    if (decoded === "") return this.root
    if (decoded.startsWith("/")) return resolvePointer(this.root, decoded)
    return findAnchor(this.root, decoded)
  }

  /** Every schema object applying to the same instance as one of `set` (deduplicated, cycle-safe). */
  expand(set: readonly unknown[]): SchemaNode[] {
    const out: SchemaNode[] = []
    const seen = new Set<SchemaNode>()
    const stack = [...set].reverse()
    while (stack.length > 0) {
      const obj = asSchemaNode(stack.pop())
      if (!obj || seen.has(obj)) continue
      seen.add(obj)
      out.push(obj)
      const next: unknown[] = []
      if (typeof obj.$ref === "string") {
        const target = this.resolveRef(obj.$ref)
        if (target !== undefined) next.push(target)
      }
      for (const combinator of COMBINATORS) {
        const branches = obj[combinator]
        if (Array.isArray(branches)) next.push(...branches)
      }
      for (const conditional of CONDITIONALS) {
        if (obj[conditional] !== undefined) next.push(obj[conditional])
      }
      for (const keyword of ["dependentSchemas", "dependencies"]) {
        const map = asSchemaNode(obj[keyword])
        if (map) next.push(...Object.values(map).filter((v) => asSchemaNode(v)))
      }
      stack.push(...next.reverse())
    }
    return out
  }

  /** Whether the instance location described by `set` is secret. */
  isSecret(set: readonly unknown[]): boolean {
    return this.expand(set).some(hasSecretMarker)
  }

  /**
   * Whether the catalog author declared the location not secret (`writeOnly: false` in an
   * applying schema) and no applying schema marks it secret.
   */
  isDeclaredNotSecret(set: readonly unknown[]): boolean {
    const expanded = this.expand(set)
    return (
      !expanded.some(hasSecretMarker) &&
      expanded.some((n) => n.writeOnly === false)
    )
  }

  /** Schemas applying to member `key` of an object validated by `set`. */
  propertySchemas(set: readonly unknown[], key: string): SchemaNode[] {
    const out: unknown[] = []
    for (const node of this.expand(set)) {
      let matched = false
      const props = asSchemaNode(node.properties)
      if (props && Object.prototype.hasOwnProperty.call(props, key)) {
        out.push(props[key])
        matched = true
      }
      const patterns = asSchemaNode(node.patternProperties)
      if (patterns) {
        for (const [pattern, sub] of Object.entries(patterns)) {
          if (patternMatches(pattern, key)) {
            out.push(sub)
            matched = true
          }
        }
      }
      if (!matched && asSchemaNode(node.additionalProperties)) {
        out.push(node.additionalProperties)
      }
    }
    // Like the server, keep only object nodes reachable by `expand` (boolean schemas drop out).
    return out.filter((v): v is SchemaNode => Boolean(asSchemaNode(v)))
  }

  /** Schemas applying to item `index` of an array validated by `set` (`undefined`: any item). */
  itemSchemas(set: readonly unknown[], index?: number): SchemaNode[] {
    const out: unknown[] = []
    for (const node of this.expand(set)) {
      const prefix = Array.isArray(node.prefixItems)
        ? node.prefixItems
        : undefined
      const items = node.items
      const tuple = Array.isArray(items) ? items : undefined
      const additional = asSchemaNode(node.additionalItems)
      if (index !== undefined) {
        if (prefix && index < prefix.length) {
          out.push(prefix[index])
        } else if (tuple) {
          if (index < tuple.length) out.push(tuple[index])
          else if (additional) out.push(additional)
        } else if (items !== undefined) {
          out.push(items)
        }
      } else {
        if (prefix) out.push(...prefix)
        if (tuple) out.push(...tuple)
        else if (items !== undefined) out.push(items)
        if (additional) out.push(additional)
      }
    }
    return out.filter((v): v is SchemaNode => Boolean(asSchemaNode(v)))
  }

  /** Schemas applying at the dot path `path` (`a.b`, list items as `list[]`; `""` is the root). */
  schemasAtPath(path: string): SchemaNode[] {
    let set: unknown[] = [this.root]
    for (const segment of splitDotPath(path)) {
      set =
        segment === "[]"
          ? this.itemSchemas(set)
          : this.propertySchemas(set, segment)
      if (set.length === 0) break
    }
    return set.filter((v): v is SchemaNode => Boolean(asSchemaNode(v)))
  }

  /** Whether the dot path lies inside a secret field (the field or an enclosing one is secret). */
  isSecretPath(path: string): boolean {
    let set: unknown[] = [this.root]
    if (this.isSecret(set)) return true
    for (const segment of splitDotPath(path)) {
      set =
        segment === "[]"
          ? this.itemSchemas(set)
          : this.propertySchemas(set, segment)
      if (this.isSecret(set)) return true
      if (set.length === 0) return false
    }
    return false
  }
}
