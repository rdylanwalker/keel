import React from "react"
import {
  AlertTriangle,
  ExternalLink,
  RotateCcw,
  RotateCw,
  Search,
  SlidersHorizontal,
} from "lucide-react"

import { CatalogItem, ReleaseSummary } from "../../types/api"
import { formatLastSynced } from "../../lib/format"
import { StatusBadge } from "../StatusBadge"
import { Badge } from "../ui/badge"
import { Button } from "../ui/button"

/** `catalog/core` id of a management add-on release (falls back to the Flux object name). */
export function managementAddonId(release: ReleaseSummary): string {
  return release.catalogId || release.name
}

export interface ManagementAddonCardProps {
  /** Management add-on as returned by `GET /api/v1/releases` (`managementAddon: true`). */
  release: ReleaseSummary
  /** `catalog/core` entry (name, description, schema for upgrades), when present. */
  addon?: CatalogItem
  reconciling: boolean
  rollingBack: boolean
  onDiagnostics: (release: ReleaseSummary) => void
  onReconcile: (release: ReleaseSummary) => void
  onUpgrade: (addon: CatalogItem) => void
  onRollback: (id: string) => void
}

const CONFLICT_HINT =
  "Defined by more than one Flux object in Git — upgrade and rollback are disabled until the duplicate is removed."

/**
 * Protected management-plane add-on in the topology map: live Flux status, desired version and
 * PR-driven Upgrade / Rollback. Never offers delete or tenant config editing.
 */
export const ManagementAddonCard: React.FC<ManagementAddonCardProps> = ({
  release,
  addon,
  reconciling,
  rollingBack,
  onDiagnostics,
  onReconcile,
  onUpgrade,
  onRollback,
}) => {
  const id = managementAddonId(release)
  const title = addon?.name || id
  const chart = release.chart || addon?.chart
  const version = release.desiredVersion || release.version
  const conflict = Boolean(release.conflict)
  return (
    <div
      data-testid={`core-addon-${id}`}
      className="flex flex-col justify-between bg-slate-950/60 border border-slate-800 hover:border-indigo-500/50 p-4 rounded-xl transition-all space-y-3"
    >
      <div className="space-y-2">
        <div className="flex flex-wrap items-start justify-between gap-2">
          <div className="min-w-0 flex-1 basis-32">
            <h4
              className="text-sm font-semibold text-white truncate"
              title={title}
            >
              {title}
            </h4>
            <p className="text-[11px] font-mono text-slate-500 truncate">
              {release.namespace}/{release.name}
              {release.addonKind ? ` · ${release.addonKind}` : ""}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge title="keel.io/protect: true">Protected</Badge>
            <span data-testid={`core-addon-status-${id}`}>
              <StatusBadge status={release.status} size="xs" />
            </span>
          </div>
        </div>

        {addon?.description && (
          <p className="text-[11px] text-slate-400 line-clamp-2">
            {addon.description}
          </p>
        )}

        {(chart || version || release.lastSynced) && (
          <div className="flex flex-wrap items-center gap-2 text-[11px] font-mono text-slate-400 pt-1">
            {chart && (
              <span className="px-2 py-0.5 rounded bg-slate-950 border border-slate-800 truncate">
                {chart}
              </span>
            )}
            {version && (
              <span
                className="px-2 py-0.5 rounded bg-slate-950 border border-slate-800 text-slate-300 shrink-0"
                title="Desired version declared in Git"
              >
                v{version.replace(/^v/i, "")}
              </span>
            )}
            {release.lastSynced && (
              <span
                className="font-sans text-slate-500"
                title={release.lastSynced}
              >
                synced {formatLastSynced(release.lastSynced)}
              </span>
            )}
          </div>
        )}

        {conflict && (
          <p
            role="note"
            data-testid={`core-addon-conflict-${id}`}
            className="flex items-start gap-1.5 rounded-lg border border-amber-500/30 bg-amber-950/40 p-2 text-[11px] text-amber-300"
          >
            <AlertTriangle
              className="h-3.5 w-3.5 shrink-0 text-amber-400"
              aria-hidden="true"
            />
            <span>{CONFLICT_HINT}</span>
          </p>
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

      <div className="pt-3 border-t border-slate-800/60 flex flex-wrap items-center gap-2">
        <Button
          variant="ghost"
          size="sm"
          aria-label={`Diagnostics for ${id}`}
          onClick={() => onDiagnostics(release)}
        >
          <Search className="h-3 w-3" aria-hidden="true" />
          <span>Diagnostics</span>
        </Button>
        <Button
          variant="ghost"
          size="sm"
          aria-label={`Reconcile ${id}`}
          disabled={reconciling}
          aria-busy={reconciling}
          onClick={() => onReconcile(release)}
        >
          <RotateCw
            aria-hidden="true"
            className={`h-3 w-3 ${reconciling ? "animate-spin" : ""}`}
          />
          <span>Reconcile</span>
        </Button>
        <Button
          variant="subtle"
          size="sm"
          aria-label={`Upgrade ${id}`}
          title={
            conflict
              ? CONFLICT_HINT
              : addon
                ? "Open an upgrade PR (version + values)"
                : "No catalog/core entry for this add-on"
          }
          disabled={conflict || !addon}
          onClick={() => addon && onUpgrade(addon)}
        >
          <SlidersHorizontal className="h-3 w-3" aria-hidden="true" />
          <span>Upgrade</span>
        </Button>
        <Button
          variant="secondary"
          size="sm"
          aria-label={`Rollback ${id}`}
          title={conflict ? CONFLICT_HINT : "Open a rollback PR"}
          disabled={conflict || rollingBack}
          aria-busy={rollingBack}
          onClick={() => onRollback(id)}
        >
          <RotateCcw
            aria-hidden="true"
            className={`h-3 w-3 ${rollingBack ? "animate-spin" : ""}`}
          />
          <span>{rollingBack ? "Rolling back..." : "Rollback"}</span>
        </Button>
      </div>
    </div>
  )
}
