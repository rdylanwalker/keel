import { fieldErrorsFromApiError, ApiError } from "@/lib/errors"

const MANIFEST_VAR_ERROR = /Manifest variable '([A-Za-z_][A-Za-z0-9_]*)' (.+)$/

/**
 * Extracts per-field errors from a failed proposal request: schema `details[]` from the
 * server, plus manifest-variable rejections (`Manifest variable 'x' …`) mapped onto field `x`.
 */
export function serverFieldErrors(err: unknown): Record<string, string[]> {
  const fields = fieldErrorsFromApiError(err)
  if (err instanceof ApiError && err.status === 400) {
    const m = MANIFEST_VAR_ERROR.exec(err.message)
    if (m) (fields[m[1]] ??= []).push(m[2])
  }
  return fields
}
