import React from "react"
import { AlertTriangle, CheckCircle2, Layers, RefreshCw } from "lucide-react"

import { ReleaseSummary, StackSummary } from "../../types/api"

const HEALTHY = new Set(["synced", "ready", "healthy"])
const RECONCILING = new Set(["reconciling", "progressing"])
const ATTENTION = new Set([
  "drift",
  "drifted",
  "degraded",
  "error",
  "failed",
  "deleting",
  "orphaned",
])

export function countByStatus(items: Array<{ status?: string }>) {
  const norm = (s?: string) => (s ?? "").toLowerCase()
  return {
    total: items.length,
    healthy: items.filter((i) => HEALTHY.has(norm(i.status))).length,
    reconciling: items.filter((i) => RECONCILING.has(norm(i.status))).length,
    attention: items.filter((i) => ATTENTION.has(norm(i.status))).length,
  }
}

const StatCard: React.FC<{
  label: string
  value: number
  valueClass: string
  iconClass: string
  icon: React.ReactNode
}> = ({ label, value, valueClass, iconClass, icon }) => (
  <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
    <div>
      <p className="text-xs text-slate-400">{label}</p>
      <p className={`text-2xl font-bold mt-1 ${valueClass}`}>{value}</p>
    </div>
    <div className={`p-3 rounded-xl ${iconClass}`}>{icon}</div>
  </div>
)

export const TopologyStats: React.FC<{
  stacks: StackSummary[]
  releases: ReleaseSummary[]
}> = ({ stacks, releases }) => {
  const c = countByStatus([...stacks, ...releases])
  return (
    <div className="grid grid-cols-2 lg:grid-cols-4 gap-3 sm:gap-4">
      <StatCard
        label="Total Stacks & Apps"
        value={c.total}
        valueClass="text-white"
        iconClass="bg-indigo-500/10 text-indigo-400"
        icon={<Layers className="h-6 w-6" />}
      />
      <StatCard
        label="Synced & Healthy"
        value={c.healthy}
        valueClass="text-emerald-400"
        iconClass="bg-emerald-500/10 text-emerald-400"
        icon={<CheckCircle2 className="h-6 w-6" />}
      />
      <StatCard
        label="Reconciling"
        value={c.reconciling}
        valueClass="text-blue-400"
        iconClass="bg-blue-500/10 text-blue-400"
        icon={
          <RefreshCw
            className={`h-6 w-6 ${c.reconciling > 0 ? "animate-spin" : ""}`}
          />
        }
      />
      <StatCard
        label="Drifted / Error / Deleting / Orphaned"
        value={c.attention}
        valueClass="text-amber-400"
        iconClass="bg-amber-500/10 text-amber-400"
        icon={<AlertTriangle className="h-6 w-6" />}
      />
    </div>
  )
}
