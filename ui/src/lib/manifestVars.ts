/**
 * Client-side mirror of `keel_core::render_manifest_template` safety rules
 * (`crates/keel-core/src/catalog.rs`). A placeholder that is the whole YAML value
 * (`key: ${var}` / `- ${var}`) accepts any value (the server quotes it); a placeholder
 * embedded inside a larger scalar (`image: repo/${tag}-x`) only accepts values that cannot
 * alter the YAML structure. Keep in sync with `validate_embedded_manifest_value`.
 */

const EMBEDDED_FORBIDDEN_CHARS = [
  '"',
  "'",
  "\\",
  "#",
  "{",
  "}",
  "[",
  "]",
  ",",
  "&",
  "*",
  "!",
  "|",
  ">",
  "%",
  "@",
  "`",
]

export interface ManifestPlaceholderUsage {
  name: string
  /** True when the placeholder is the entire YAML value (server quotes it safely). */
  wholeScalar: boolean
}

function isValidVarName(name: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name)
}

const isWs = (c: string | undefined) => c !== undefined && /\s/.test(c)

/** Mirrors `placeholder_is_whole_scalar(prefix, suffix)`. */
export function placeholderIsWholeScalar(
  prefix: string,
  suffix: string
): boolean {
  const suffixOk =
    suffix.trim() === "" ||
    (isWs(suffix[0]) && suffix.trimStart().startsWith("#"))
  if (!suffixOk) return false
  let rest = prefix.trimStart()
  let sawSeqDash = false
  while (rest.startsWith("-")) {
    const after = rest.slice(1)
    if (!isWs(after[0])) break
    sawSeqDash = true
    rest = after.trimStart()
  }
  if (rest === "") return sawSeqDash
  return isWs(rest[rest.length - 1]) && rest.trimEnd().endsWith(":")
}

/** Scans a manifest template for `${var}` / `${var:=default}` placeholders and their placement. */
export function scanManifestPlaceholders(
  template: string
): ManifestPlaceholderUsage[] {
  const out: ManifestPlaceholderUsage[] = []
  let i = 0
  while (i < template.length) {
    if (
      template[i] === "$" &&
      template[i + 1] === "{" &&
      i + 2 < template.length
    ) {
      const rest = template.slice(i + 2)
      const relEnd = rest.search(/[}\n]/)
      if (relEnd >= 0 && rest[relEnd] === "}") {
        const end = i + 2 + relEnd
        const inner = template.slice(i + 2, end)
        const sep = inner.indexOf(":=")
        const name = sep >= 0 ? inner.slice(0, sep) : inner
        if (isValidVarName(name)) {
          const lineStart = template.lastIndexOf("\n", i - 1) + 1
          const nl = template.indexOf("\n", end + 1)
          const lineEnd = nl === -1 ? template.length : nl
          out.push({
            name,
            wholeScalar: placeholderIsWholeScalar(
              template.slice(lineStart, i),
              template.slice(end + 1, lineEnd)
            ),
          })
          i = end + 1
          continue
        }
      }
    }
    i += 1
  }
  return out
}

/** Mirrors `validate_embedded_manifest_value`; returns a reason or `null` when safe. */
export function embeddedValueProblem(value: string): string | null {
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/.test(value)) {
    return "must not contain newlines or control characters"
  }
  const bad = EMBEDDED_FORBIDDEN_CHARS.filter((c) => value.includes(c))
  if (bad.length > 0) {
    return `cannot contain ${bad.map((c) => `"${c}"`).join(" ")} because it is inserted inside a larger YAML value`
  }
  if (value.includes(": ") || value.endsWith(":")) {
    return "must not contain ': ' or end with ':' because it is inserted inside a larger YAML value"
  }
  if (/^[-? ]/.test(value) || value.endsWith(" ")) {
    return "must not start with '-', '?' or a space, or end with a space"
  }
  return null
}

/**
 * Validates top-level form values against how each variable is used in `template`.
 * Returns `{ varName: [message] }` for values the server would reject.
 */
export function validateManifestValues(
  template: string | undefined,
  values: Record<string, unknown> | undefined
): Record<string, string[]> {
  const errors: Record<string, string[]> = {}
  if (!template || !values) return errors
  for (const usage of scanManifestPlaceholders(template)) {
    if (usage.wholeScalar) continue
    const value = values[usage.name]
    if (value === undefined || value === null) continue
    let problem: string | null = null
    if (typeof value === "string") {
      problem = embeddedValueProblem(value)
    } else if (typeof value === "object") {
      problem =
        "is a structured value and may only be used as a whole YAML value"
    }
    if (problem && !(errors[usage.name] ?? []).includes(problem)) {
      ;(errors[usage.name] ??= []).push(problem)
    }
  }
  return errors
}
