import React from "react"
import { RefreshCw } from "lucide-react"

interface StatusBadgeProps {
  status?: string
  size?: "sm" | "xs"
  className?: string
  /**
   * Inside a narrow `@container` (e.g. a data table on a phone) collapse to a coloured dot
   * (plus spinner) and keep the label as tooltip + screen-reader text; the full pill returns
   * once the container is at least `@md` (28rem). Only use inside a container-query context.
   */
  collapsible?: boolean
}

export const StatusBadge: React.FC<StatusBadgeProps> = ({
  status,
  size = "sm",
  className = "",
  collapsible = false,
}) => {
  const raw = status ?? ""
  const norm = raw.toLowerCase()

  let badgeClass = "bg-slate-500/10 text-slate-300 border-slate-500/20"
  let label = raw || "Unknown"
  let showSpinner = false

  if (norm === "synced" || norm === "ready" || norm === "healthy") {
    badgeClass = "bg-emerald-500/10 text-emerald-400 border-emerald-500/20"
    label =
      norm === "ready" ? "Ready" : norm === "healthy" ? "Healthy" : "Synced"
  } else if (norm === "reconciling" || norm === "progressing") {
    badgeClass = "bg-blue-500/10 text-blue-400 border-blue-500/20"
    label = norm === "progressing" ? "Progressing" : "Reconciling"
    showSpinner = true
  } else if (norm === "deleting") {
    badgeClass = "bg-orange-500/10 text-orange-300 border-orange-500/30"
    label = "Deleting"
    showSpinner = true
  } else if (norm === "orphaned") {
    badgeClass = "bg-purple-500/10 text-purple-300 border-purple-500/30"
    label = "Orphaned"
  } else if (norm === "pending") {
    badgeClass = "bg-slate-500/10 text-slate-300 border-slate-500/20"
    label = "Pending"
  } else if (norm === "error" || norm === "failed") {
    badgeClass = "bg-red-500/10 text-red-400 border-red-500/20"
    label = norm === "failed" ? "Failed" : "Error"
  } else if (norm === "drift" || norm === "drifted" || norm === "degraded") {
    badgeClass = "bg-amber-500/10 text-amber-400 border-amber-500/20"
    label = norm === "degraded" ? "Degraded" : "Drift Detected"
  }

  const sizeClass =
    size === "xs" ? "px-2 py-0.5 text-[10px]" : "px-2.5 py-0.5 text-[11px]"

  return (
    <span
      title={
        norm === "orphaned"
          ? "Live in the cluster but no longer declared in Git"
          : collapsible
            ? label
            : undefined
      }
      className={`inline-flex items-center whitespace-nowrap space-x-1 rounded-full font-medium border ${
        collapsible
          ? `px-1.5 py-1 @md:py-0.5 ${size === "xs" ? "@md:px-2 text-[10px]" : "@md:px-2.5 text-[11px]"}`
          : sizeClass
      } ${badgeClass} ${className}`.trim()}
    >
      {showSpinner && <RefreshCw className="h-3 w-3 animate-spin shrink-0" />}
      {collapsible && !showSpinner && (
        <span
          aria-hidden="true"
          className="h-2 w-2 shrink-0 rounded-full bg-current @md:hidden"
        />
      )}
      <span className={collapsible ? "sr-only @md:not-sr-only" : undefined}>
        {label}
      </span>
    </span>
  )
}
