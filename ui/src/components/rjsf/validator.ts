/**
 * Eval-free `@rjsf/utils` `ValidatorType` backed by the interpreting `@cfworker/json-schema`
 * validator.
 *
 * `keel-server` ships a strict CSP (`script-src 'self' 'wasm-unsafe-eval'`, no `'unsafe-eval'`),
 * which breaks `@rjsf/validator-ajv8` because AJV compiles schemas with `new Function`. Catalog
 * schemas are loaded from Git at runtime, so build-time precompilation is not an option either.
 * This validator interprets schemas directly and reshapes the raw output into AJV-compatible
 * errors (`name`/`params`/`property`/`schemaPath`) so rjsf error rendering and the friendly
 * messages in `@/lib/formErrors` keep working unchanged.
 */
import {
  dereference,
  validate,
  type OutputUnit,
  type Schema,
  type SchemaDraft,
} from "@cfworker/json-schema"
import {
  ANY_OF_KEY,
  createErrorHandler,
  getByPath,
  getDefaultFormState,
  getUiOptions,
  ID_KEY,
  JUNK_OPTION_ID,
  ONE_OF_KEY,
  PROPERTIES_KEY,
  REF_KEY,
  ROOT_SCHEMA_PREFIX,
  toErrorSchema,
  unwrapErrorHandler,
  validationDataMerge,
  type CustomValidator,
  type ErrorSchema,
  type ErrorTransformer,
  type FormContextType,
  type RJSFSchema,
  type RJSFValidationError,
  type StrictRJSFSchema,
  type UiSchema,
  type ValidationData,
  type ValidatorType,
} from "@rjsf/utils"

/** AJV-shaped raw error (the shape `validator-ajv8`'s `rawValidation` used to return). */
export interface RawValidationError {
  instancePath: string
  keyword: string
  params: Record<string, unknown>
  schemaPath: string
  message: string
  parentSchema?: Record<string, unknown>
}

type Lookup = Record<string, Schema | boolean>
type JsonObject = Record<string, unknown>

const ROOT_URI = "https://keel.invalid/__rjsf_root.json"
const TARGET_URI = "https://keel.invalid/__rjsf_target.json"

/** cfworker keywords that only wrap nested errors; AJV reports just the nested leaf errors. */
const WRAPPER_KEYWORDS = new Set([
  "$ref",
  "$recursiveRef",
  "properties",
  "patternProperties",
  "additionalProperties",
  "unevaluatedProperties",
  "propertyNames",
  "dependentSchemas",
  "items",
  "prefixItems",
  "additionalItems",
  "unevaluatedItems",
  "allOf",
])

/** Keys whose values are data, not subschemas; `$ref` strings inside them are left alone. */
const DATA_KEYS = new Set(["enum", "const", "default", "examples"])

const isObject = (v: unknown): v is JsonObject =>
  typeof v === "object" && v !== null && !Array.isArray(v)

function cloneJson<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T
}

/**
 * Converts rjsf form data into a plain JSON instance. cfworker throws on `undefined` (rjsf keeps
 * cleared fields as `undefined`), so object keys holding `undefined` are dropped — matching AJV,
 * which treats them as missing for `required`.
 */
export function toJsonInstance(value: unknown): unknown {
  if (value === undefined || typeof value === "function") return undefined
  if (typeof value === "symbol") return undefined
  if (typeof value === "bigint") return Number(value)
  if (value instanceof Date) return value.toISOString()
  if (Array.isArray(value)) {
    return value.map((item) => {
      const json = toJsonInstance(item)
      return json === undefined ? null : json
    })
  }
  if (isObject(value)) {
    const out: JsonObject = {}
    for (const [key, item] of Object.entries(value)) {
      const json = toJsonInstance(item)
      if (json !== undefined) out[key] = json
    }
    return out
  }
  return value
}

/** Rewrites local `#…` (and rjsf `__rjsf_rootSchema#…`) refs so they resolve against the root. */
function rewriteRefs(node: unknown): unknown {
  if (Array.isArray(node)) return node.map(rewriteRefs)
  if (!isObject(node)) return node
  const out: JsonObject = {}
  for (const [key, value] of Object.entries(node)) {
    if (key === REF_KEY && typeof value === "string") {
      const local = value.startsWith(ROOT_SCHEMA_PREFIX)
        ? value.slice(ROOT_SCHEMA_PREFIX.length)
        : value
      out[key] = local.startsWith("#") ? `${ROOT_URI}${local}` : value
    } else {
      out[key] = DATA_KEYS.has(key) ? value : rewriteRefs(value)
    }
  }
  return out
}

function draftFor(schema: unknown): SchemaDraft {
  const declared = isObject(schema) ? String(schema.$schema ?? "") : ""
  if (declared.includes("draft-04")) return "4"
  if (declared.includes("2020-12")) return "2020-12"
  // AJV 8 evaluates keywords alongside `$ref` for every draft; cfworker's draft 7 mode does not,
  // so draft-06/07 and unversioned schemas use 2019-09 semantics (same keyword set otherwise).
  return "2019-09"
}

interface PreparedRoot {
  schema: Schema
  lookup: Lookup
  draft: SchemaDraft
}

interface PreparedTarget {
  schema: Schema
  lookup: Lookup
}

function decodeSegment(segment: string): string {
  let decoded = segment
  try {
    decoded = decodeURIComponent(segment)
  } catch {
    // keep the raw segment
  }
  return decoded.replace(/~1/g, "/").replace(/~0/g, "~")
}

/** `#/a/b~1c` (URI-encoded JSON pointer) -> AJV `instancePath` `/a/b~1c`. */
function toInstancePath(location: string): string {
  const segments = location.replace(/^#/, "").split("/").slice(1)
  return segments
    .map((s) => decodeSegment(s).replace(/~/g, "~0").replace(/\//g, "~1"))
    .map((s) => `/${s}`)
    .join("")
}

/** Walks a cfworker `keywordLocation` from `schema`, following `$ref` hops through `lookup`. */
function schemaAt(
  schema: Schema,
  lookup: Lookup,
  location: string
): JsonObject | undefined {
  let node: unknown = schema
  for (const raw of location.replace(/^#/, "").split("/").slice(1)) {
    if (!isObject(node) && !Array.isArray(node)) return undefined
    const segment = decodeSegment(raw)
    if (segment === "$ref" && isObject(node)) {
      const uri =
        (node as Schema).__absolute_ref__ ?? (node as Schema).$ref ?? ""
      node = lookup[uri]
    } else {
      node = (node as JsonObject)[segment]
    }
  }
  return isObject(node) ? node : undefined
}

/** Schema owning the keyword at `keywordLocation` (location minus its last segment). */
function parentSchemaAt(
  schema: Schema,
  lookup: Lookup,
  keywordLocation: string
): JsonObject | undefined {
  const idx = keywordLocation.lastIndexOf("/")
  return idx <= 0
    ? isObject(schema)
      ? (schema as JsonObject)
      : undefined
    : schemaAt(schema, lookup, keywordLocation.slice(0, idx))
}

const quoted = (re: RegExp, text: string): string | undefined =>
  re.exec(text)?.[1]

/** Maps one cfworker leaf error to AJV's `keyword`/`params`/`message` contract. */
function toAjvShape(
  unit: OutputUnit,
  parent: JsonObject | undefined,
  next: OutputUnit | undefined
): Omit<RawValidationError, "schemaPath" | "parentSchema"> | undefined {
  const instancePath = toInstancePath(unit.instanceLocation)
  const value = parent?.[unit.keyword]
  const base = { instancePath, keyword: unit.keyword }
  switch (unit.keyword) {
    case "type": {
      const type = Array.isArray(value) ? value.join(",") : String(value)
      return { ...base, params: { type }, message: `must be ${type}` }
    }
    case "required": {
      const missingProperty =
        quoted(/required property "(.*)"\.$/, unit.error) ?? ""
      return {
        ...base,
        params: { missingProperty },
        message: `must have required property '${missingProperty}'`,
      }
    }
    case "minLength":
    case "maxLength":
    case "minItems":
    case "maxItems":
    case "minProperties":
    case "maxProperties": {
      const noun = unit.keyword.endsWith("Length")
        ? "characters"
        : unit.keyword.endsWith("Items")
          ? "items"
          : "properties"
      const dir = unit.keyword.startsWith("min") ? "fewer" : "more"
      return {
        ...base,
        params: { limit: value },
        message: `must NOT have ${dir} than ${value} ${noun}`,
      }
    }
    case "minimum":
    case "maximum": {
      const isMin = unit.keyword === "minimum"
      const exclusive =
        parent?.[isMin ? "exclusiveMinimum" : "exclusiveMaximum"] === true
      const comparison = isMin
        ? exclusive
          ? ">"
          : ">="
        : exclusive
          ? "<"
          : "<="
      return {
        ...base,
        params: { comparison, limit: value },
        message: `must be ${comparison} ${value}`,
      }
    }
    case "exclusiveMinimum":
    case "exclusiveMaximum": {
      const comparison = unit.keyword === "exclusiveMinimum" ? ">" : "<"
      return {
        ...base,
        params: { comparison, limit: value },
        message: `must be ${comparison} ${value}`,
      }
    }
    case "multipleOf":
      return {
        ...base,
        params: { multipleOf: value },
        message: `must be multiple of ${value}`,
      }
    case "pattern":
      return {
        ...base,
        params: { pattern: value },
        message: `must match pattern "${value}"`,
      }
    case "format":
      return {
        ...base,
        params: { format: value },
        message: `must match format "${value}"`,
      }
    case "enum":
      return {
        ...base,
        params: { allowedValues: value },
        message: "must be equal to one of the allowed values",
      }
    case "const":
      return {
        ...base,
        params: { allowedValue: value },
        message: "must be equal to constant",
      }
    case "uniqueItems": {
      const m = /indexes (\d+) and (\d+)/.exec(unit.error)
      const [i, j] = m ? [Number(m[1]), Number(m[2])] : [undefined, undefined]
      return {
        ...base,
        params: { i, j },
        message: `must NOT have duplicate items (items ## ${j} and ${i} are identical)`,
      }
    }
    case "oneOf":
      return {
        ...base,
        params: { passingSchemas: null },
        message: "must match exactly one schema in oneOf",
      }
    case "anyOf":
      return { ...base, params: {}, message: "must match a schema in anyOf" }
    case "not":
      return { ...base, params: {}, message: "must NOT be valid" }
    case "if": {
      const failingKeyword = unit.error.includes('"else"') ? "else" : "then"
      return {
        ...base,
        params: { failingKeyword },
        message: `must match "${failingKeyword}" schema`,
      }
    }
    case "dependencies":
    case "dependentRequired": {
      const m = /has "(.*)" but does not have "(.*)"\.$/.exec(unit.error)
      if (!m) return undefined // schema-form dependencies: nested errors carry the detail
      return {
        ...base,
        params: {
          property: m[1],
          missingProperty: m[2],
          deps: m[2],
          depsCount: 1,
        },
        message: `must have property ${m[2]} when property ${m[1]} is present`,
      }
    }
    case "additionalProperties": {
      // Only reached for `additionalProperties: false` (see `collectRawErrors`).
      const additionalProperty = next
        ? decodeSegment(next.instanceLocation.split("/").pop() ?? "")
        : ""
      return {
        ...base,
        params: { additionalProperty },
        message: "must NOT have additional properties",
      }
    }
    case "false":
      return { ...base, params: {}, message: "boolean schema is false" }
    default:
      return {
        ...base,
        params: {},
        message: unit.error.replace(/\.$/, "").toLowerCase(),
      }
  }
}

function collectRawErrors(
  units: OutputUnit[],
  schema: Schema,
  lookup: Lookup
): RawValidationError[] {
  const out: RawValidationError[] = []
  for (let idx = 0; idx < units.length; idx++) {
    const unit = units[idx]
    const next = units[idx + 1]
    const falseAdditional =
      unit.keyword === "additionalProperties" && next?.keyword === "false"
    if (WRAPPER_KEYWORDS.has(unit.keyword) && !falseAdditional) continue
    if (falseAdditional) idx++ // swallow the nested `false` boolean-schema error
    const parent =
      unit.keyword === "false"
        ? undefined
        : parentSchemaAt(schema, lookup, unit.keywordLocation)
    const shaped = toAjvShape(unit, parent, next)
    if (!shaped) continue
    out.push({
      ...shaped,
      schemaPath: unit.keywordLocation,
      parentSchema: parent,
    })
  }
  return out
}

/** Port of `validator-ajv8`'s duplicate filtering for errors raised inside anyOf/oneOf branches. */
function filterDuplicateErrors(
  errors: RJSFValidationError[]
): RJSFValidationError[] {
  return errors.reduce<RJSFValidationError[]>((acc, err) => {
    const { message, schemaPath = "" } = err
    const anyOfIndex = schemaPath.indexOf(`/${ANY_OF_KEY}/`)
    const oneOfIndex = schemaPath.indexOf(`/${ONE_OF_KEY}/`)
    let prefix: string | undefined
    if (anyOfIndex > 0) prefix = schemaPath.substring(0, anyOfIndex)
    else if (oneOfIndex > 0) prefix = schemaPath.substring(0, oneOfIndex)
    const dup = prefix
      ? acc.find(
          (e) => e.message === message && e.schemaPath?.startsWith(prefix)
        )
      : undefined
    if (!dup) acc.push(err)
    return acc
  }, [])
}

function titleFor(
  uiSchema: unknown,
  rootSchema: unknown,
  parentSchema: JsonObject | undefined,
  property: string,
  child?: string
): string | undefined {
  const parts = property.replace(/^\./, "").split(".").filter(Boolean)
  const path = child ? [...parts, child] : parts
  const uiTitle = getUiOptions(getByPath(uiSchema, path) as UiSchema).title
  if (uiTitle) return uiTitle
  if (child) {
    const parentTitle = getByPath(parentSchema, [
      PROPERTIES_KEY,
      child,
      "title",
    ])
    if (typeof parentTitle === "string") return parentTitle
    const rootPath = path.flatMap((p) => [PROPERTIES_KEY, p])
    const rootTitle = getByPath(rootSchema, [...rootPath, "title"])
    return typeof rootTitle === "string" ? rootTitle : undefined
  }
  return typeof parentSchema?.title === "string"
    ? parentSchema.title
    : undefined
}

/** AJV-shaped raw errors -> rjsf `RJSFValidationError`s (mirrors `validator-ajv8`). */
export function toRjsfErrors(
  raw: RawValidationError[],
  uiSchema?: unknown,
  rootSchema?: unknown
): RJSFValidationError[] {
  const errors = raw.map((e): RJSFValidationError => {
    let property = e.instancePath.replace(/\//g, ".")
    let message = e.message
    let stack = `${property} ${message}`.trim()
    let title = ""
    const named = [e.params.missingProperty, e.params.additionalProperty].find(
      (v): v is string => typeof v === "string" && v !== ""
    )
    if (named) {
      const t = titleFor(uiSchema, rootSchema, e.parentSchema, property, named)
      if (t) {
        message = message.replace(`'${named}'`, `'${t}'`)
        title = t
      }
      stack = message
    } else {
      const t = titleFor(uiSchema, rootSchema, e.parentSchema, property)
      if (t) {
        stack = `'${t}' ${message}`.trim()
        title = t
      }
    }
    if (typeof e.params.missingProperty === "string") {
      property = property
        ? `${property}.${e.params.missingProperty}`
        : e.params.missingProperty
    }
    return {
      name: e.keyword,
      property,
      message,
      params: e.params,
      stack,
      schemaPath: e.schemaPath,
      title,
    }
  })
  return filterDuplicateErrors(errors)
}

/**
 * Interpreting (no `eval` / `new Function`) rjsf validator. Prepared schemas are cached per
 * schema object identity; nothing is ever compiled to code.
 */
export class InterpretingValidator<
  T = unknown,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = FormContextType,
> implements ValidatorType<T, S, F> {
  private roots = new WeakMap<object, PreparedRoot>()
  private targets = new WeakMap<object, WeakMap<object, PreparedTarget>>()

  private prepareRoot(rootSchema: S): PreparedRoot {
    const cached = this.roots.get(rootSchema)
    if (cached) return cached
    const schema = cloneJson(rootSchema) as Schema
    const lookup = dereference(schema, Object.create(null), new URL(ROOT_URI))
    const prepared = { schema, lookup, draft: draftFor(rootSchema) }
    this.roots.set(rootSchema, prepared)
    return prepared
  }

  private prepareTarget(schema: S, root: PreparedRoot, rootKey: S) {
    let perRoot = this.targets.get(rootKey)
    if (!perRoot) {
      perRoot = new WeakMap()
      this.targets.set(rootKey, perRoot)
    }
    const cached = perRoot.get(schema)
    if (cached) return cached
    const target = rewriteRefs(cloneJson(schema)) as Schema
    const lookup = dereference(
      target,
      Object.assign(Object.create(null) as Lookup, root.lookup),
      new URL(TARGET_URI)
    )
    const prepared = { schema: target, lookup }
    perRoot.set(schema, prepared)
    return prepared
  }

  private run(schema: S, rootSchema: S, formData: unknown) {
    const root = this.prepareRoot(rootSchema)
    const { schema: target, lookup } =
      schema === rootSchema
        ? root
        : this.prepareTarget(schema, root, rootSchema)
    const instance = toJsonInstance(formData)
    const result = validate(
      instance === undefined ? null : instance,
      target,
      root.draft,
      lookup,
      false
    )
    return { result, target, lookup }
  }

  rawValidation<Result = RawValidationError>(
    schema: S,
    formData?: T
  ): { errors?: Result[]; validationError?: Error } {
    try {
      const { result, target, lookup } = this.run(schema, schema, formData)
      if (result.valid) return { errors: undefined }
      return {
        errors: collectRawErrors(result.errors, target, lookup) as Result[],
      }
    } catch (e) {
      return { validationError: e instanceof Error ? e : new Error(String(e)) }
    }
  }

  validateFormData(
    formData: T | undefined,
    schema: S,
    customValidate?: CustomValidator<T, S, F>,
    transformErrors?: ErrorTransformer<T, S, F>,
    uiSchema?: UiSchema<T, S, F>
  ): ValidationData<T> {
    const { errors: raw = [], validationError } = this.rawValidation(
      schema,
      formData
    )
    let errors = toRjsfErrors(raw, uiSchema, schema)
    if (validationError) {
      errors = [...errors, { stack: validationError.message }]
    }
    if (typeof transformErrors === "function") {
      errors = transformErrors(errors, uiSchema)
    }
    let errorSchema = toErrorSchema<T>(errors)
    if (validationError) {
      errorSchema = {
        ...errorSchema,
        $schema: { __errors: [validationError.message] },
      } as ErrorSchema<T>
    }
    if (typeof customValidate !== "function") return { errors, errorSchema }
    // Include form data with undefined values, which custom validation expects.
    const newFormData = getDefaultFormState<T, S, F>(
      this,
      schema,
      formData,
      schema,
      true
    ) as T
    const errorHandler = customValidate(
      newFormData,
      createErrorHandler<T>(newFormData),
      uiSchema,
      errorSchema
    )
    return validationDataMerge<T>(
      { errors, errorSchema },
      unwrapErrorHandler<T>(errorHandler)
    )
  }

  isValid(schema: S, formData: T | undefined, rootSchema: S): boolean {
    if (isObject(schema) && schema[ID_KEY] === JUNK_OPTION_ID) return false
    try {
      return this.run(schema, rootSchema, formData).result.valid
    } catch (e) {
      console.warn("Error validating against schema:", e)
      return false
    }
  }

  reset(): void {
    this.roots = new WeakMap()
    this.targets = new WeakMap()
  }
}

export function createValidator<
  T = unknown,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = FormContextType,
>(): ValidatorType<T, S, F> {
  return new InterpretingValidator<T, S, F>()
}

/** Shared default instance used by `SchemaForm`. */
const validator = createValidator<Record<string, unknown>, RJSFSchema>()
export default validator
