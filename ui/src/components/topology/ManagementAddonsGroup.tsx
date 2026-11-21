import React, { useEffect, useState } from "react"
import { ShieldCheck } from "lucide-react"

import { CatalogItem, Proposal, ReleaseSummary } from "../../types/api"
import { getCoreAddons, rollbackManagementAddon } from "../../lib/api"
import { errorMessage } from "../../lib/errors"
import { isDemoMode } from "../../lib/utils"
import { DEFAULT_CORE_ADDONS } from "../../lib/demoFixtures"
import { DeployInstanceModal } from "../DeployInstanceModal"
import { ConfirmDialog } from "../ui/confirm-dialog"
import { ManagementAddonCard, managementAddonId } from "./ManagementAddonCard"

export interface ManagementAddonsGroupProps {
  /** Management add-on releases (`managementAddon: true`), cluster admins only. */
  addons: ReleaseSummary[]
  /** `<namespace>/<name>` of the release currently being reconciled. */
  reconcilingKey: string | null
  onDiagnostics: (release: ReleaseSummary) => void
  onReconcile: (release: ReleaseSummary) => void
  /** Called with the PR opened by an upgrade or rollback. */
  onProposal: (
    proposal: Proposal,
    action: "upgrade" | "rollback",
    id: string
  ) => void
  onError: (message: string | null) => void
}

/**
 * Rendered for every cluster admin (cards only when `addons` is non-empty).
 *
 * "Management Plane" group of the topology releases section: protected core add-ons with live
 * status (refreshed with the rest of the topology), Diagnostics, Reconcile, and PR-driven
 * Upgrade (catalog/core schema form) / Rollback through `/api/v1/management/addons/{id}`.
 */
export const ManagementAddonsGroup: React.FC<ManagementAddonsGroupProps> = ({
  addons,
  reconcilingKey,
  onDiagnostics,
  onReconcile,
  onProposal,
  onError,
}) => {
  const demo = isDemoMode()
  const [coreAddons, setCoreAddons] = useState<CatalogItem[]>(() =>
    demo ? DEFAULT_CORE_ADDONS : []
  )
  const [selectedAddon, setSelectedAddon] = useState<CatalogItem | null>(null)
  const [confirmRollbackId, setConfirmRollbackId] = useState<string | null>(
    null
  )
  const [rollingBackId, setRollingBackId] = useState<string | null>(null)

  useEffect(() => {
    if (demo) return
    let cancelled = false
    getCoreAddons()
      .then((items) => {
        if (!cancelled) setCoreAddons(items ?? [])
      })
      .catch(() => {
        // Upgrade stays disabled without a catalog/core entry; status still renders.
      })
    return () => {
      cancelled = true
    }
  }, [demo])

  const catalogById = new Map(coreAddons.map((a) => [a.id, a] as const))

  const handleRollback = async (id: string) => {
    setRollingBackId(id)
    onError(null)
    try {
      onProposal(await rollbackManagementAddon(id), "rollback", id)
    } catch (err) {
      onError(errorMessage(err, `Failed to create rollback PR for ${id}`))
    } finally {
      setRollingBackId(null)
    }
  }

  // The dialogs below render outside the group so an open upgrade form or rollback
  // confirmation survives a refresh that (temporarily) returns no add-ons.
  return (
    <>
      {addons.length > 0 && (
        <div
          data-testid="management-addons-group"
          role="group"
          aria-labelledby="management-addons-heading"
          className="space-y-3 rounded-xl border border-indigo-500/20 bg-indigo-950/10 p-3 sm:p-4"
        >
          <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-1">
            <h4
              id="management-addons-heading"
              className="text-xs font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-2"
            >
              <ShieldCheck
                className="h-4 w-4 text-indigo-400"
                aria-hidden="true"
              />
              Management Plane
            </h4>
            <span className="text-[11px] text-slate-500">
              Protected Flux add-ons in flux-system &middot; changes open
              reviewed PRs
            </span>
          </div>
          <div className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-3">
            {addons.map((release) => {
              const id = managementAddonId(release)
              return (
                <ManagementAddonCard
                  key={`${release.namespace}/${release.name}/${release.addonKind ?? ""}`}
                  release={release}
                  addon={catalogById.get(id)}
                  reconciling={
                    reconcilingKey === `${release.namespace}/${release.name}`
                  }
                  rollingBack={rollingBackId === id}
                  onDiagnostics={onDiagnostics}
                  onReconcile={onReconcile}
                  onUpgrade={setSelectedAddon}
                  onRollback={setConfirmRollbackId}
                />
              )
            })}
          </div>
        </div>
      )}

      <ConfirmDialog
        open={confirmRollbackId !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmRollbackId(null)
        }}
        title={`Roll back ${confirmRollbackId ?? ""}?`}
        description="This opens a pull request reverting the management add-on manifest to its previous Git revision. Nothing changes until the PR is reviewed and merged."
        confirmLabel="Open Rollback PR"
        onConfirm={() => {
          const id = confirmRollbackId
          setConfirmRollbackId(null)
          if (id) void handleRollback(id)
        }}
      />

      <DeployInstanceModal
        isOpen={Boolean(selectedAddon)}
        onClose={() => setSelectedAddon(null)}
        catalogItem={selectedAddon}
        managementAddon={selectedAddon}
        tenant="keel-system"
        onProposalCreated={(proposal) => {
          if (selectedAddon) onProposal(proposal, "upgrade", selectedAddon.id)
        }}
        onCatalogItemUpdated={(updated) => {
          setCoreAddons((prev) =>
            prev.map((a) => (a.id === updated.id ? updated : a))
          )
          setSelectedAddon(updated)
        }}
      />
    </>
  )
}
