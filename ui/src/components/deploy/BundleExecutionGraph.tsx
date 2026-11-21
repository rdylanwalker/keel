import React from "react"

import { CatalogBundleItem } from "@/types/api"

export const BundleExecutionGraph: React.FC<{ items: CatalogBundleItem[] }> = ({
  items,
}) => (
  <div className="bg-indigo-950/20 border border-indigo-500/30 rounded-xl p-3.5 space-y-2">
    <div className="text-[11px] font-semibold uppercase tracking-wider text-indigo-300">
      Bundle Execution Graph ({items.length} items)
    </div>
    <div className="flex flex-wrap gap-2">
      {items.map((bi) => (
        <div
          key={bi.id}
          className="text-[11px] font-mono bg-slate-950 border border-slate-800 rounded-lg px-2.5 py-1.5 text-slate-200 flex flex-wrap items-center gap-1.5"
        >
          <span className="px-1.5 py-0.5 rounded bg-indigo-500/20 text-indigo-300 text-[10px] uppercase">
            {bi.kind}
          </span>
          <span className="font-semibold text-white">{bi.id}</span>
          <span className="text-slate-500">({bi.catalogRef})</span>
          {bi.canary && (
            <span className="px-1.5 py-0.5 rounded bg-emerald-500/20 text-emerald-300 text-[10px] uppercase">
              canary
            </span>
          )}
          {bi.dependsOn && bi.dependsOn.length > 0 && (
            <span className="text-amber-300 text-[10px]">
              ← {bi.dependsOn.join(", ")}
            </span>
          )}
        </div>
      ))}
    </div>
  </div>
)
