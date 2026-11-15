import React from "react"
import { RefreshCw } from "lucide-react"

interface StatusBadgeProps {
  status?: string
  size?: "sm" | "xs"
  className?: string
}

export const StatusBadge: React.FC<StatusBadgeProps> = ({
  status,
  size = "sm",
  className = "",
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
      className={`inline-flex items-center whitespace-nowrap space-x-1 rounded-full font-medium border ${sizeClass} ${badgeClass} ${className}`.trim()}
    >
      {showSpinner && <RefreshCw className="h-3 w-3 animate-spin shrink-0" />}
      <span>{label}</span>
    </span>
  )
}
