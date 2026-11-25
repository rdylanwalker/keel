/**
 * Helpers for secret (`writeOnly` / `format: password`) catalog schema fields so the UI never
 * echoes secret material in previews and always renders secrets with password widgets.
 */

import {
  SchemaResolver,
  asSchemaNode,
  hasSecretMarker,
  splitDotPath,
  type SchemaNode,
} from "./schemaResolver"

export { splitDotPath }

export const SECRET_MASK = "********"

type UiSchema = Record<string, unknown>

/**
 * Whether `node` itself carries a secret marker (`writeOnly: true` or `format: password`).
 * Node-local only: no `$ref` or combinator resolution (use {@link SchemaResolver} for that).
 */
export const isSecretSchemaNode = hasSecretMarker

function maskNode(
  resolver: SchemaResolver,
  set: readonly unknown[],
  value: unknown
): unknown {
  if (value === undefined || value === null) return value
  if (resolver.isSecret(set)) return SECRET_MASK
  if (Array.isArray(value)) {
    return value.map((item, i) =>
      maskNode(resolver, resolver.itemSchemas(set, i), item)
    )
  }
  if (typeof value === "object") {
    const out: Record<string, unknown> = {}
    for (const [key, child] of Object.entries(value as SchemaNode)) {
      out[key] = maskNode(resolver, resolver.propertySchemas(set, key), child)
    }
    return out
  }
  return value
}

/**
 * Returns a copy of `data` where every value whose schema is secret (`$ref`, combinators and
 * pattern/additional properties resolved like the server) is replaced by {@link SECRET_MASK}.
 * Used for display-only previews; never for submitted payloads.
 */
export function maskSecretValues<T>(schema: unknown, data: T): T {
  const root = asSchemaNode(schema)
  return maskNode(new SchemaResolver(root ?? {}), root ? [root] : [], data) as T
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
  const props = asSchemaNode(schema.properties)
  if (props) {
    for (const [key, child] of Object.entries(props)) {
      const childNode = asSchemaNode(child)
      if (!childNode) continue
      const merged = secretUiNode(
        childNode,
        asSchemaNode(out[key]) ?? {},
        editMode
      )
      if (Object.keys(merged).length > 0) out[key] = merged
    }
  }
  const items = asSchemaNode(schema.items)
  if (items) {
    const merged = secretUiNode(items, asSchemaNode(out.items) ?? {}, editMode)
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
  const root = asSchemaNode(schema)
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
  const node = asSchemaNode(schema)
  if (!node || isSecretSchemaNode(node)) return schema
  const out: SchemaNode = { ...node }
  const props = asSchemaNode(node.properties)
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
  if (asSchemaNode(node.items)) out.items = relaxSecretRequirements(node.items)
  for (const comb of ["allOf", "anyOf", "oneOf"] as const) {
    if (Array.isArray(node[comb])) {
      out[comb] = (node[comb] as unknown[]).map((b) =>
        relaxSecretRequirements(b)
      )
    }
  }
  return out as T
}

// ---------------------------------------------------------------------------------------------
// Name-based secret heuristic (AST-2): mirror of `crates/keel-core/src/secret_heuristic.rs`.
// A value the schema does not declare secret is still treated as secret (stored encrypted with
// SOPS) when its key looks like one. `src/test/fixtures/secret-key-heuristic.json` is the corpus
// both the Rust and the vitest suites run, so the two implementations cannot drift apart.
// ---------------------------------------------------------------------------------------------

/** Words that make a key look like it holds a secret (matched against the normalized key). */
export const SECRET_KEY_WORDS = [
  "password",
  "passwd",
  "pwd",
  "passphrase",
  "secret",
  "token",
  "apikey",
  "credential",
  "privatekey",
  "signingkey",
  "encryptionkey",
] as const

/** Normalized key endings in `pass` that are ordinary words, not a password. */
export const PASS_NON_SECRETS = [
  "bypass",
  "compass",
  "overpass",
  "underpass",
  "surpass",
  "trespass",
  "onepass",
  "multipass",
] as const

/** Normalized key endings that name, locate or describe a secret instead of holding it. */
export const REFERENCE_SUFFIXES = [
  "name",
  "names",
  "ref",
  "refs",
  "selector",
  "namespace",
  "store",
  "class",
  "provider",
  "backend",
  "version",
  "file",
  "files",
  "filename",
  "path",
  "paths",
  "dir",
  "directory",
  "mount",
  "url",
  "uri",
  "endpoint",
  "header",
  "issuer",
  "audience",
  "type",
  "kind",
  "mode",
  "policy",
  "format",
  "length",
  "ttl",
  "expiry",
  "expiration",
  "lifetime",
  "duration",
  "timeout",
  "interval",
  "seconds",
  "secs",
  "minutes",
  "hours",
  "days",
  "count",
  "size",
  "limit",
  "port",
  "enabled",
  "prefix",
  "suffix",
  "template",
  "tokenizer",
] as const

/** What precedes a trailing `key` when the key names an entry inside a Secret (`passwordKey`). */
export const KEY_NAME_SUBJECTS = [
  "password",
  "passwd",
  "pwd",
  "passphrase",
  "token",
  "credential",
  "credentials",
  "apikey",
] as const

/** The key reduced to lowercase ASCII letters and digits (`DB_PASSWORD` -> `dbpassword`). */
function normalizeKey(key: string): string {
  return key.replace(/[^A-Za-z0-9]/g, "").toLowerCase()
}

/** Whether the normalized key names a secret word, or ends in `pass` (`dbpass`). */
function hasSecretWord(n: string): boolean {
  return (
    SECRET_KEY_WORDS.some((w) => n.includes(w)) ||
    (n.endsWith("pass") && !PASS_NON_SECRETS.some((w) => n.endsWith(w)))
  )
}

/**
 * Mirrors `keel_core::looks_like_secret_key`: whether a value under `key` looks like a secret.
 * Keys with `/` (annotation/label keys) never match; a dotted key (`ssl.keystore.password`) is
 * judged by its last dot segment.
 */
export function looksLikeSecretKey(key: string): boolean {
  if (key.includes("/")) return false
  const n = normalizeKey(key.slice(key.lastIndexOf(".") + 1))
  if (!hasSecretWord(n)) return false
  if (n.startsWith("existing") || REFERENCE_SUFFIXES.some((s) => n.endsWith(s)))
    return false
  if (n.endsWith("key")) {
    const rest = n.slice(0, -3)
    if (KEY_NAME_SUBJECTS.some((s) => rest.endsWith(s))) return false
    if (rest.endsWith("secret")) {
      const before = rest.slice(0, -"secret".length)
      if (hasSecretWord(before)) return false
    }
  }
  return true
}

/** Whether a map named `parent` holds annotations or labels (`podAnnotations`, `commonLabels`). */
function isMetadataMap(parent: string): boolean {
  const n = normalizeKey(parent)
  return n.endsWith("annotations") || n.endsWith("labels")
}

/**
 * Mirrors `keel_core::looks_like_secret_key_under`: {@link looksLikeSecretKey} for a key of the
 * map under `parent` (`undefined`: the root or a list item); never for annotation/label keys.
 */
export function looksLikeSecretKeyUnder(
  parent: string | undefined,
  key: string
): boolean {
  return (
    !(parent !== undefined && isMetadataMap(parent)) && looksLikeSecretKey(key)
  )
}

/** A concrete value location (`keel_core::SecretValueLocation`). */
export interface SecretValueLocation {
  /** JSON Pointer (`/auth/password`). */
  pointer: string
  /** Dot path (`auth.password`, list items as `users[].password`). */
  path: string
}

/**
 * The catalog author's opt-out: `writeOnly: false` (and no applying schema marks it secret).
 * An `enum`/`const` is no opt-out (it may sit in one `anyOf` branch only).
 */
function declaredNotSecret(
  resolver: SchemaResolver,
  set: readonly unknown[]
): boolean {
  return resolver.isDeclaredNotSecret(set)
}

/** A scalar the heuristic stores as a secret: a non-empty string or a number. */
function isSecretScalar(value: unknown): boolean {
  return (
    (typeof value === "string" && value !== "") || typeof value === "number"
  )
}

function joinDot(prefix: string, key: string): string {
  return prefix ? `${prefix}.${key}` : key
}

function escapePointerToken(token: string): string {
  return token.replace(/~/g, "~0").replace(/\//g, "~1")
}

function collectCandidates(
  resolver: SchemaResolver,
  set: readonly unknown[],
  value: unknown,
  pointer: string,
  path: string,
  parent: string | undefined,
  key: string | undefined,
  out: SecretValueLocation[]
): void {
  if (resolver.isSecret(set)) return
  const namedSecret = () =>
    key !== undefined &&
    looksLikeSecretKeyUnder(parent, key) &&
    !declaredNotSecret(resolver, set)
  if (typeof value === "string" || typeof value === "number") {
    if (isSecretScalar(value) && namedSecret()) out.push({ pointer, path })
    return
  }
  if (Array.isArray(value)) {
    if (
      value.length > 0 &&
      value.every((v) => typeof v === "string" || typeof v === "number") &&
      value.some(isSecretScalar) &&
      namedSecret()
    ) {
      out.push({ pointer, path })
      return
    }
    value.forEach((item, i) =>
      collectCandidates(
        resolver,
        resolver.itemSchemas(set, i),
        item,
        `${pointer}/${i}`,
        `${path}[]`,
        undefined,
        undefined,
        out
      )
    )
    return
  }
  const obj = asSchemaNode(value)
  if (!obj) return
  for (const [childKey, child] of Object.entries(obj)) {
    collectCandidates(
      resolver,
      resolver.propertySchemas(set, childKey),
      child,
      `${pointer}/${escapePointerToken(childKey)}`,
      joinDot(path, childKey),
      key,
      childKey,
      out
    )
  }
}

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0
}

/**
 * Mirrors `keel_core::heuristic_secret_candidates`: every value of `values` the name heuristic
 * treats as secret, sorted. Values `schema` already makes secret are not listed; user
 * overrides are not applied here.
 */
export function heuristicSecretCandidates(
  schema: unknown,
  values: unknown
): SecretValueLocation[] {
  const resolver = new SchemaResolver(schema)
  const out: SecretValueLocation[] = []
  collectCandidates(
    resolver,
    [schema],
    values,
    "",
    "",
    undefined,
    undefined,
    out
  )
  return out.sort(
    (a, b) =>
      compareStrings(a.pointer, b.pointer) || compareStrings(a.path, b.path)
  )
}

const SCALAR_TYPES = ["string", "number", "integer"]

/**
 * Whether a schema set can hold a string or number (or a list of them) the heuristic would
 * match.
 */
function mayHoldSecretScalar(
  resolver: SchemaResolver,
  set: SchemaNode[]
): boolean {
  const nodes = resolver.expand(set)
  const types = nodes.flatMap((n) =>
    Array.isArray(n.type) ? n.type : n.type === undefined ? [] : [n.type]
  )
  if (types.length === 0) return true
  if (types.some((t) => SCALAR_TYPES.includes(t as string))) return true
  if (types.includes("array")) {
    const items = resolver.expand(resolver.itemSchemas(set))
    const itemTypes = items.flatMap((n) =>
      Array.isArray(n.type) ? n.type : n.type === undefined ? [] : [n.type]
    )
    return (
      itemTypes.length === 0 ||
      itemTypes.some((t) => SCALAR_TYPES.includes(t as string))
    )
  }
  return false
}

/**
 * Form hint (UI only): dot paths of schema-named fields the heuristic will treat as secret once
 * filled (key looks like a secret, the schema does not make it secret, the author did not opt it
 * out, and it can hold a string or number). Lets the form render a password input and the warning before
 * the user types; the submitted values are judged by {@link heuristicSecretCandidates}, exactly
 * like the server.
 */
export function heuristicSecretSchemaPaths(schema: unknown): string[] {
  const resolver = new SchemaResolver(schema)
  const out = new Set<string>()
  const visited = new Set<SchemaNode>()
  const walk = (
    set: SchemaNode[],
    path: string,
    parent: string | undefined,
    depth: number
  ) => {
    if (depth > 32 || resolver.isSecret(set)) return
    const expanded = resolver.expand(set)
    if (expanded.length > 0 && expanded.every((n) => visited.has(n))) return
    expanded.forEach((n) => visited.add(n))
    const keys = new Set<string>()
    for (const node of expanded) {
      const props = asSchemaNode(node.properties)
      if (props) Object.keys(props).forEach((k) => keys.add(k))
    }
    for (const key of keys) {
      const childSet = resolver.propertySchemas(set, key)
      const childPath = joinDot(path, key)
      if (resolver.isSecret(childSet)) continue
      if (
        looksLikeSecretKeyUnder(parent, key) &&
        !declaredNotSecret(resolver, childSet) &&
        mayHoldSecretScalar(resolver, childSet)
      ) {
        out.add(childPath)
        continue
      }
      walk(childSet, childPath, key, depth + 1)
    }
    const items = resolver.itemSchemas(set)
    if (items.length > 0) walk(items, `${path}[]`, undefined, depth + 1)
  }
  walk([asSchemaNode(schema) ?? {}], "", undefined, 0)
  return [...out].sort(compareStrings)
}

/** Replaces the value at dot `path` (`a.b`, `list[].c`) of `data` by {@link SECRET_MASK}. */
function maskAtPath(data: unknown, segments: string[]): unknown {
  if (segments.length === 0) {
    return data === undefined || data === null ? data : SECRET_MASK
  }
  const [segment, ...rest] = segments
  if (segment === "[]") {
    return Array.isArray(data)
      ? data.map((item) => maskAtPath(item, rest))
      : data
  }
  const obj = asSchemaNode(data)
  if (!obj || !(segment in obj)) return data
  return { ...obj, [segment]: maskAtPath(obj[segment], rest) }
}

/** Returns a copy of `data` with the values at the dot `paths` replaced by {@link SECRET_MASK}. */
export function maskValuesAtPaths<T>(data: T, paths: readonly string[]): T {
  return paths.reduce<unknown>(
    (acc, path) => maskAtPath(acc, splitDotPath(path)),
    data
  ) as T
}

/** How the form presents a field the name heuristic matches. */
export interface HeuristicSecretField {
  /** Dot path, also the `nonSecretPaths` entry. */
  path: string
  /** The user marked it not secret (stored in plaintext). */
  notSecret: boolean
  /** Rendered from the schema (password input) rather than only from a typed value. */
  fromSchema: boolean
  /**
   * Edit only: newly marked not secret while blank. A stored secret is shown blank, so its value
   * must be re-entered to be stored in plaintext (the server rejects the blank).
   */
  reenter?: boolean
}

/** The value at dot `path` (objects only; `undefined` for list paths or a missing value). */
export function valueAtDotPath(data: unknown, path: string): unknown {
  let node: unknown = data
  for (const segment of splitDotPath(path)) {
    const obj = asSchemaNode(node)
    if (segment === "[]" || !obj) return undefined
    node = obj[segment]
  }
  return node
}

/** Blank as the server judges a submitted secret: absent, `null`, `""`, `[]` or `{}`. */
export function isBlankValue(value: unknown): boolean {
  if (value === undefined || value === null || value === "") return true
  if (Array.isArray(value)) return value.length === 0
  const obj = asSchemaNode(value)
  return obj !== undefined && Object.keys(obj).length === 0
}

/** `ui:options` key carrying {@link HeuristicSecretField} to the field template. */
export const HEURISTIC_SECRET_UI_OPTION = "keelHeuristicSecret"

/**
 * Adds the name-heuristic presentation to a uiSchema: a password input (unless marked not secret
 * or the catalog picked a widget), the "keep existing" placeholder in edit mode, and the
 * {@link HEURISTIC_SECRET_UI_OPTION} the field template turns into the inline warning and the
 * "Not a secret" toggle.
 */
export function withHeuristicSecretUi(
  uiSchema: UiSchema,
  fields: readonly HeuristicSecretField[],
  options: { editMode?: boolean } = {}
): UiSchema {
  let out: UiSchema = { ...uiSchema }
  for (const field of fields) {
    const segments = splitDotPath(field.path).map((s) =>
      s === "[]" ? "items" : s
    )
    out = setUiAt(out, segments, (node) => {
      const next: UiSchema = { ...node }
      const prevOptions = asSchemaNode(next["ui:options"]) ?? {}
      next["ui:options"] = {
        ...prevOptions,
        [HEURISTIC_SECRET_UI_OPTION]: field,
      }
      if (field.fromSchema && !field.notSecret) {
        if (next["ui:widget"] === undefined) next["ui:widget"] = "password"
        if (options.editMode && next["ui:placeholder"] === undefined) {
          next["ui:placeholder"] = KEEP_EXISTING_SECRET_PLACEHOLDER
        }
      }
      return next
    })
  }
  return out
}

function setUiAt(
  ui: UiSchema,
  segments: string[],
  update: (node: UiSchema) => UiSchema
): UiSchema {
  if (segments.length === 0) return update(ui)
  const [head, ...rest] = segments
  return { ...ui, [head]: setUiAt(asSchemaNode(ui[head]) ?? {}, rest, update) }
}

/**
 * Edit mode: a blank heuristic secret keeps its stored ciphertext (like a declared secret), so
 * schema-named heuristic fields are not `required`. Returns a copy of `schema` with each dot
 * path (objects only) removed from its parent's `required` list.
 */
export function relaxRequiredAtPaths<T>(
  schema: T,
  paths: readonly string[]
): T {
  let out: unknown = schema
  for (const path of paths) {
    const segments = splitDotPath(path)
    if (segments.includes("[]")) continue
    out = relaxAt(out, segments)
  }
  return out as T
}

function relaxAt(node: unknown, segments: string[]): unknown {
  const obj = asSchemaNode(node)
  if (!obj || segments.length === 0) return node
  const [head, ...rest] = segments
  const next: SchemaNode = { ...obj }
  if (rest.length === 0) {
    if (Array.isArray(obj.required)) {
      next.required = (obj.required as unknown[]).filter((k) => k !== head)
    }
  } else {
    const props = asSchemaNode(obj.properties)
    if (props && head in props) {
      next.properties = { ...props, [head]: relaxAt(props[head], rest) }
    }
  }
  return next
}
