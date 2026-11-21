import React from "react"
import {
  AlertOctagon,
  Layers,
  Loader2,
  MinusCircle,
  PlusCircle,
  RefreshCcw,
} from "lucide-react"

import { PreviewReport, ResourceDiff } from "../../types/api"

const OP_STYLES: Record<
  string,
  {
    label: string
    icon?: React.ComponentType<{ className?: string }>
    tone: string
  }
> = {
  create: {
    label: "Create",
    icon: PlusCircle,
    tone: "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20",
  },
  update: {
    label: "Update",
    icon: RefreshCcw,
    tone: "bg-blue-500/10 text-blue-400 border border-blue-500/20",
  },
  delete: {
    label: "Delete",
    icon: MinusCircle,
    tone: "bg-red-500/10 text-red-400 border border-red-500/20",
  },
  replace: {
    label: "Replace",
    icon: AlertOctagon,
    tone: "bg-amber-500/10 text-amber-400 border border-amber-500/20",
  },
  drift: {
    label: "Drift",
    icon: AlertOctagon,
    tone: "bg-amber-500/10 text-amber-400 border border-amber-500/20",
  },
}

export const OpBadge: React.FC<{ op: ResourceDiff["op"] }> = ({ op }) => {
  const style = OP_STYLES[(op ?? "").toLowerCase()]
  if (!style) {
    return (
      <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-slate-800 text-slate-400">
        <span>{op}</span>
      </span>
    )
  }
  const Icon = style.icon
  return (
    <span
      className={`inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold ${style.tone}`}
    >
      {Icon && <Icon className="h-3 w-3" />}
      <span>{style.label}</span>
    </span>
  )
}

const SUMMARY_CELLS: {
  key: keyof PreviewReport["summary"]
  label: string
  prefix: string
  tone: string
}[] = [
  { key: "create", label: "Create", prefix: "+", tone: "text-emerald-400" },
  { key: "update", label: "Update", prefix: "~", tone: "text-blue-400" },
  { key: "delete", label: "Delete", prefix: "-", tone: "text-red-400" },
  { key: "replace", label: "Replace", prefix: "!", tone: "text-amber-400" },
  { key: "same", label: "Unchanged", prefix: "=", tone: "text-slate-400" },
]

export const PreviewSummaryBar: React.FC<{
  summary: PreviewReport["summary"]
}> = ({ summary }) => (
  <div className="grid grid-cols-2 sm:grid-cols-5 gap-3 text-center">
    {SUMMARY_CELLS.map((cell) => (
      <div
        key={cell.key}
        className="bg-slate-950 border border-slate-800 p-3 rounded-xl"
      >
        <p className="text-[10px] text-slate-500 uppercase font-mono">
          {cell.label}
        </p>
        <p className={`text-lg font-bold mt-0.5 ${cell.tone}`}>
          {cell.prefix}
          {summary[cell.key] ?? 0}
        </p>
      </div>
    ))}
  </div>
)

export const ResourceDiffTree: React.FC<{
  resources: ResourceDiff[] | undefined
  loading: boolean
  hasPreview: boolean
}> = ({ resources, loading, hasPreview }) => (
  <div className="space-y-3">
    <div className="flex items-center justify-between">
      <h4 className="text-xs font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-2">
        <Layers className="h-4 w-4 text-indigo-400" />
        Resource Diff Tree
      </h4>
      {loading && (
        <span className="text-xs text-indigo-400 flex items-center gap-1 font-mono">
          <Loader2 className="h-3 w-3 animate-spin" /> Fetching preview...
        </span>
      )}
    </div>

    <div className="space-y-2 max-h-96 overflow-y-auto pr-1">
      {(resources ?? []).map((res, idx) => (
        <div
          key={`${res.urn}-${idx}`}
          className="bg-slate-950 border border-slate-800 rounded-xl p-4 space-y-2 hover:border-slate-700 transition-colors"
        >
          <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-2">
            <div className="flex items-center space-x-2 min-w-0">
              <OpBadge op={res.op} />
              <span className="text-xs font-bold text-slate-200 truncate">
                {res.name}
              </span>
            </div>
            <span className="text-[10px] font-mono text-slate-500 truncate max-w-xs">
              {res.resource_type}
            </span>
          </div>

          <p className="text-[11px] font-mono text-slate-500 truncate">
            {res.urn}
          </p>

          {(res.changed_fields ?? []).length > 0 && (
            <div className="flex items-center flex-wrap gap-1.5 pt-1">
              <span className="text-[10px] text-slate-500 font-mono">
                Changed fields:
              </span>
              {res.changed_fields.map((field) => (
                <span
                  key={field}
                  className="text-[10px] font-mono px-2 py-0.5 rounded bg-slate-900 border border-slate-800 text-indigo-300"
                >
                  {field}
                </span>
              ))}
            </div>
          )}
        </div>
      ))}

      {hasPreview && (resources ?? []).length === 0 && (
        <div className="py-8 text-center text-xs text-slate-500">
          No resource changes found in this preview.
        </div>
      )}
    </div>
  </div>
)
