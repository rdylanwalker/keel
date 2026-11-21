import React from "react"
import { Server } from "lucide-react"

import { StackSummary } from "../../types/api"
import { formatLastSynced } from "../../lib/format"
import { StatusBadge } from "../StatusBadge"
import { Badge } from "../ui/badge"

/**
 * Read-only summary of the management control plane, rendered ONLY from a real
 * management stack returned by the API (never fabricated, OPEN-7).
 */
export const ManagementPlaneStrip: React.FC<{
  stack: StackSummary
  onNavigateToProviders?: () => void
}> = ({ stack, onNavigateToProviders }) => (
  <div
    data-testid="management-plane-strip"
    className="bg-slate-900 border border-slate-800 rounded-xl px-4 py-3"
  >
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex flex-wrap items-center gap-2.5 text-xs">
        <span className="inline-flex items-center gap-1.5 font-semibold text-white">
          <Server className="h-3.5 w-3.5 text-indigo-400" />
          Management Control Plane
        </span>
        {stack.protect && <Badge>protect: true</Badge>}
        <StatusBadge status={stack.status} size="xs" />
        {stack.provider && (
          <span className="text-[10px] uppercase tracking-wider font-mono text-slate-400">
            {stack.provider}
          </span>
        )}
        {typeof stack.resourceCount === "number" && (
          <span className="text-[11px] text-slate-400">
            {stack.resourceCount} resources
          </span>
        )}
        {stack.lastSynced && (
          <span className="text-[11px] text-slate-500">
            synced {formatLastSynced(stack.lastSynced)}
          </span>
        )}
      </div>
      {onNavigateToProviders && (
        <button
          type="button"
          onClick={onNavigateToProviders}
          className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 px-2 py-1 cursor-pointer"
        >
          Providers &rarr;
        </button>
      )}
    </div>
  </div>
)
