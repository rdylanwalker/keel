import { ManagementAddonStatus } from "../../types/api"

export type BadgeVariant =
  "success" | "warning" | "destructive" | "secondary" | "default"

export const NOT_TRACKED_LABEL = "Not tracked (installed by bootstrap)"

/**
 * Badge for a management add-on. Status comes only from the live HelmRelease with the same
 * namespace/name (never from a chart-name match); anything else is "Not tracked".
 */
export function addonStatusBadge(status?: ManagementAddonStatus): {
  label: string
  variant: BadgeVariant
} {
  if (!status || !status.tracked || !status.status) {
    return { label: NOT_TRACKED_LABEL, variant: "secondary" }
  }
  const raw = status.status.toLowerCase()
  if (raw === "ready" || raw === "synced" || raw === "healthy") {
    return { label: "Ready", variant: "success" }
  }
  if (raw === "reconciling" || raw === "progressing" || raw === "pending") {
    return { label: "Reconciling", variant: "warning" }
  }
  if (raw === "deleting") {
    return { label: "Deleting", variant: "warning" }
  }
  if (raw === "error" || raw === "failed" || raw === "degraded") {
    return { label: "Failed", variant: "destructive" }
  }
  return { label: status.status, variant: "secondary" }
}

export interface ManagementHealth {
  label: string
  tone: "healthy" | "warning" | "error" | "unknown"
  detail?: string
}

/** Derives management-plane health from real add-on statuses (never assumed healthy). */
export function deriveManagementHealth(
  statuses: ManagementAddonStatus[] | null,
  liveStatusAvailable: boolean
): ManagementHealth {
  if (!statuses) {
    return { label: "Status unknown", tone: "unknown" }
  }
  if (!liveStatusAvailable) {
    return {
      label: "Status unknown",
      tone: "unknown",
      detail: "No live cluster connection",
    }
  }
  const tracked = statuses.filter((s) => s.tracked)
  if (tracked.length === 0) {
    return {
      label: "Status unknown",
      tone: "unknown",
      detail: "No tracked add-ons",
    }
  }
  const labels = tracked.map((s) => addonStatusBadge(s).label)
  const failed = labels.filter((l) => l === "Failed").length
  if (failed > 0) {
    return {
      label: "Degraded",
      tone: "error",
      detail: `${failed} of ${tracked.length} add-ons failing`,
    }
  }
  const pending = labels.filter((l) => l !== "Ready").length
  if (pending > 0) {
    return {
      label: "Reconciling",
      tone: "warning",
      detail: `${pending} of ${tracked.length} add-ons not ready`,
    }
  }
  return {
    label: "Healthy",
    tone: "healthy",
    detail: `${tracked.length} tracked add-ons ready`,
  }
}

/** Maps open `keel/providers/toggle-<id>-<ts>` PR branches to provider ids. */
export function extractPendingProviderPrs(
  proposals: { id: string; branch: string; status: string }[]
): Record<string, { id: string; branch: string }> {
  const map: Record<string, { id: string; branch: string }> = {}
  for (const pr of proposals) {
    if (pr.status !== "open") continue
    const match = /^keel\/providers\/toggle-([a-z0-9-]+)-\d+$/.exec(pr.branch)
    if (match) {
      map[match[1]] = { id: pr.id, branch: pr.branch }
    }
  }
  return map
}
