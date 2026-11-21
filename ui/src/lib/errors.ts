import { isDemoMode } from "./utils"

/** A single schema-validation failure reported by the server (`details[]`). */
export interface ApiFieldError {
  /** JSON Pointer to the invalid value (e.g. `/adminEmail`, `""` for root). */
  fieldPath: string
  schemaPath?: string
  message: string
}

/** Structured error raised for non-2xx Keel API responses. */
export class ApiError extends Error {
  readonly status: number
  readonly correlationId?: string
  readonly details: ApiFieldError[]

  constructor(
    status: number,
    message: string,
    options: { correlationId?: string; details?: ApiFieldError[] } = {}
  ) {
    super(message)
    this.name = "ApiError"
    this.status = status
    this.correlationId = options.correlationId
    this.details = options.details ?? []
  }

  get isServerError(): boolean {
    return this.status >= 500
  }
}

/** Error thrown instead of issuing a network request while `?demo=true` is active. */
export class DemoModeError extends ApiError {
  constructor() {
    super(0, "Not available in demo mode (no live API calls are made).")
    this.name = "DemoModeError"
  }
}

const STATUS_FALLBACKS: Record<number, string> = {
  400: "The request was rejected as invalid.",
  401: "Your session has expired. Please sign in again.",
  403: "You do not have permission to perform this action.",
  404: "The requested resource was not found.",
  409: "The request conflicts with the current state.",
  422: "Some values are invalid.",
  429: "Too many requests. Please wait a moment and try again.",
}

function fallbackMessage(status: number, statusText?: string): string {
  if (STATUS_FALLBACKS[status]) return STATUS_FALLBACKS[status]
  if (status >= 500) {
    return `The Keel server could not complete the request (HTTP ${status}${
      statusText ? ` ${statusText}` : ""
    }).`
  }
  return statusText || `Request failed (HTTP ${status}).`
}

function stripCorrelationSuffix(message: string): string {
  return message.replace(/\s*\(correlation id: [^)]+\)\s*$/i, "").trim()
}

/**
 * Parses an API error body (`{ error, details?, correlationId? }` JSON envelope, plain text,
 * or empty) into an {@link ApiError} with a human-readable message.
 */
export function parseApiError(
  status: number,
  body: string | undefined,
  statusText?: string
): ApiError {
  const text = (body ?? "").trim()
  if (text.startsWith("{")) {
    try {
      const parsed = JSON.parse(text) as {
        error?: unknown
        message?: unknown
        correlationId?: unknown
        details?: unknown
      }
      const raw =
        typeof parsed.error === "string"
          ? parsed.error
          : typeof parsed.message === "string"
            ? parsed.message
            : ""
      const details: ApiFieldError[] = Array.isArray(parsed.details)
        ? parsed.details
            .filter(
              (d): d is Record<string, unknown> =>
                typeof d === "object" && d !== null
            )
            .map((d) => ({
              fieldPath: String(d.fieldPath ?? d.field_path ?? ""),
              schemaPath:
                d.schemaPath !== undefined || d.schema_path !== undefined
                  ? String(d.schemaPath ?? d.schema_path)
                  : undefined,
              message: String(d.message ?? ""),
            }))
        : []
      const correlationId =
        typeof parsed.correlationId === "string"
          ? parsed.correlationId
          : undefined
      const message = stripCorrelationSuffix(raw) || fallbackMessage(status)
      return new ApiError(status, message, { correlationId, details })
    } catch {
      // fall through to plain-text handling
    }
  }
  if (text && !text.startsWith("<")) {
    return new ApiError(status, text)
  }
  return new ApiError(status, fallbackMessage(status, statusText))
}

/** Returns a user-facing message for any thrown value. */
export function errorMessage(err: unknown, fallback: string): string {
  if (err instanceof ApiError) {
    return err.correlationId && err.isServerError
      ? `${err.message} (reference: ${err.correlationId})`
      : err.message
  }
  if (err instanceof Error && err.message) return err.message
  return fallback
}

/**
 * Maps server `details[]` JSON Pointer paths (`/a/b`) onto rjsf-style dotted field ids
 * (`a.b`) so they can be shown next to the matching form field.
 */
export function fieldErrorsFromApiError(
  err: unknown
): Record<string, string[]> {
  if (!(err instanceof ApiError)) return {}
  const out: Record<string, string[]> = {}
  for (const d of err.details) {
    const key = d.fieldPath
      .replace(/^\//, "")
      .split("/")
      .filter(Boolean)
      .map((seg) => seg.replace(/~1/g, "/").replace(/~0/g, "~"))
      .join(".")
    ;(out[key] ??= []).push(d.message)
  }
  return out
}

/** True when an error is a 401 that should be silently ignored in demo mode. */
export function isIgnorableDemoError(err: unknown): boolean {
  if (err instanceof DemoModeError) return true
  if (!isDemoMode()) return false
  if (err instanceof ApiError) return err.status === 401 || err.status === 0
  return err instanceof Error && /\b401\b/.test(err.message)
}

/**
 * Converts an error into the message a view should display, or `null` when the error is an
 * expected demo-mode 401 that must not be surfaced (shared by all data-loading views).
 */
export function visibleLoadError(
  err: unknown,
  fallback: string
): string | null {
  if (isIgnorableDemoError(err)) return null
  return errorMessage(err, fallback)
}
