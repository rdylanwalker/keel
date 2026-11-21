/** Renders an ISO timestamp as a compact relative age (`just now`, `5m ago`, `2d ago`). */
export function formatLastSynced(
  value?: string,
  now: number = Date.now()
): string {
  if (!value || !value.trim()) {
    return "—"
  }
  const parsed = Date.parse(value)
  if (Number.isNaN(parsed)) {
    return value
  }
  const diffSec = Math.max(0, Math.floor((now - parsed) / 1000))
  if (diffSec < 60) return "just now"
  const diffMin = Math.floor(diffSec / 60)
  if (diffMin < 60) return `${diffMin}m ago`
  const diffHr = Math.floor(diffMin / 60)
  if (diffHr < 24) return `${diffHr}h ago`
  return `${Math.floor(diffHr / 24)}d ago`
}
