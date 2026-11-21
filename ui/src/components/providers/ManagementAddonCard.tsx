import React from "react"
import { ExternalLink, RotateCcw, SlidersHorizontal } from "lucide-react"

import { CatalogItem, ManagementAddonStatus } from "../../types/api"
import { Badge } from "../ui/badge"
import { Button } from "../ui/button"
import { addonStatusBadge } from "./addonStatus"

export interface ManagementAddonCardProps {
  id: string
  /** Catalog `catalog/core` entry (name, description, schema for upgrades), when present. */
  addon?: CatalogItem
  /** Live status matched by namespace + name. */
  status?: ManagementAddonStatus
  canManage: boolean
  rollingBack: boolean
  onUpgrade?: (addon: CatalogItem) => void
  onRollback?: (id: string) => void
}

export const ManagementAddonCard: React.FC<ManagementAddonCardProps> = ({
  id,
  addon,
  status,
  canManage,
  rollingBack,
  onUpgrade,
  onRollback,
}) => {
  const badge = addonStatusBadge(status)
  const chart = status?.chart || addon?.chart
  const version = status?.desiredVersion || addon?.version
  return (
    <div
      data-testid={`core-addon-${id}`}
      className="flex flex-col justify-between bg-slate-900 border border-slate-800 hover:border-indigo-500/50 p-4 rounded-xl transition-all space-y-3"
    >
      <div className="space-y-2">
        <div className="flex flex-wrap items-start justify-between gap-2">
          <div className="min-w-0 flex-1 basis-32">
            <h4
              className="text-sm font-semibold text-white truncate"
              title={addon?.name || id}
            >
              {addon?.name || id}
            </h4>
            <p className="text-[11px] font-mono text-slate-500 truncate">
              {status ? `${status.namespace}/${status.name}` : id}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge title="keel.io/protect: true">Protected</Badge>
            <Badge
              variant={badge.variant}
              data-testid={`core-addon-status-${id}`}
              className="whitespace-nowrap"
            >
              {badge.label}
            </Badge>
          </div>
        </div>

        {addon?.description && (
          <p className="text-[11px] text-slate-400 line-clamp-2">
            {addon.description}
          </p>
        )}

        {(chart || version) && (
          <div className="flex items-center gap-2 text-[11px] font-mono text-slate-400 pt-1">
            {chart && (
              <span className="px-2 py-0.5 rounded bg-slate-950 border border-slate-800 truncate">
                {chart}
              </span>
            )}
            {version && (
              <span className="px-2 py-0.5 rounded bg-slate-950 border border-slate-800 text-slate-300 shrink-0">
                v{version.replace(/^v/i, "")}
              </span>
            )}
          </div>
        )}

        {addon?.serviceUrl && (
          <div className="bg-slate-950 p-2.5 rounded-lg border border-slate-800 text-xs">
            <p className="text-[10px] uppercase tracking-wider text-slate-500">
              Service
            </p>
            <a
              href={addon.serviceUrl}
              target="_blank"
              rel="noreferrer"
              title={addon.serviceUrl}
              className="mt-0.5 flex min-w-0 max-w-full items-center gap-1.5 text-[11px] font-mono text-indigo-400 hover:text-indigo-300"
            >
              <span className="min-w-0 truncate">{addon.serviceUrl}</span>
              <ExternalLink className="h-3 w-3 shrink-0" aria-hidden="true" />
            </a>
          </div>
        )}
      </div>

      {canManage && (
        <div className="pt-3 border-t border-slate-800/60 flex flex-wrap items-center justify-between gap-2">
          {addon && onUpgrade ? (
            <Button variant="subtle" size="sm" onClick={() => onUpgrade(addon)}>
              <SlidersHorizontal className="h-3 w-3" aria-hidden="true" />
              <span>Upgrade / Configure</span>
            </Button>
          ) : (
            <span />
          )}
          {onRollback && (
            <Button
              variant="secondary"
              size="sm"
              disabled={rollingBack}
              aria-busy={rollingBack}
              onClick={() => onRollback(id)}
            >
              <RotateCcw
                aria-hidden="true"
                className={`h-3 w-3 ${rollingBack ? "animate-spin" : ""}`}
              />
              <span>{rollingBack ? "Rolling back..." : "Rollback PR"}</span>
            </Button>
          )}
        </div>
      )}
    </div>
  )
}
