import { useCallback, useEffect, useMemo, useState } from "react"

import { CatalogItem, Proposal, ReleaseSummary } from "../../types/api"
import { getCoreAddons, rollbackManagementAddon } from "../../lib/api"
import { errorMessage } from "../../lib/errors"
import { demoFixtures } from "../../lib/demo"

/** `catalog/core` id of a management add-on release (falls back to the Flux object name). */
export function managementAddonId(release: ReleaseSummary): string {
  return release.catalogId || release.name
}

/** Shown when an add-on id is defined by more than one Flux object in Git. */
export const MANAGEMENT_ADDON_CONFLICT_HINT =
  "Defined by more than one Flux object in Git — upgrade and rollback are disabled until the duplicate is removed."

export type ManagementAddonProposalHandler = (
  proposal: Proposal,
  action: "upgrade" | "rollback",
  id: string
) => void

export interface ManagementAddonActionsOptions {
  /** Cluster admin with a live session (or demo mode); `getCoreAddons` is only called then. */
  enabled: boolean
  demo: boolean
  /** Called with the PR opened by an upgrade or rollback. */
  onProposal: ManagementAddonProposalHandler
  onError: (message: string | null) => void
}

/**
 * State of the management add-on Upgrade / Rollback flows, owned by the topology view rather
 * than by table rows, so an open upgrade form or rollback confirmation survives a releases
 * refresh that fails or (temporarily) returns no add-ons. Render with `<ManagementAddonDialogs>`.
 */
export interface ManagementAddonActions {
  /** `catalog/core` entries by id (name, description, schema for upgrades). */
  catalogById: ReadonlyMap<string, CatalogItem>
  /** Add-on id whose rollback PR is being created. */
  rollingBackId: string | null
  /** Opens the upgrade form for a `catalog/core` entry. */
  openUpgrade: (addon: CatalogItem) => void
  /** Asks for confirmation before opening a rollback PR. */
  requestRollback: (id: string) => void
  selectedAddon: CatalogItem | null
  closeUpgrade: () => void
  confirmRollbackId: string | null
  cancelRollback: () => void
  confirmRollback: () => void
  onUpgradeProposal: (proposal: Proposal) => void
  onCatalogItemUpdated: (updated: CatalogItem) => void
}

export function useManagementAddonActions({
  enabled,
  demo,
  onProposal,
  onError,
}: ManagementAddonActionsOptions): ManagementAddonActions {
  const [coreAddons, setCoreAddons] = useState<CatalogItem[]>(() =>
    demo ? demoFixtures().DEFAULT_CORE_ADDONS : []
  )
  const [selectedAddon, setSelectedAddon] = useState<CatalogItem | null>(null)
  const [confirmRollbackId, setConfirmRollbackId] = useState<string | null>(
    null
  )
  const [rollingBackId, setRollingBackId] = useState<string | null>(null)

  // Fetched once per admin session; Upgrade stays disabled without a catalog/core entry.
  useEffect(() => {
    if (demo || !enabled) return
    let cancelled = false
    getCoreAddons()
      .then((items) => {
        if (!cancelled) setCoreAddons(items ?? [])
      })
      .catch(() => {
        // Status and Rollback still work without the catalog/core entries.
      })
    return () => {
      cancelled = true
    }
  }, [demo, enabled])

  const catalogById = useMemo(
    () => new Map(coreAddons.map((a) => [a.id, a] as const)),
    [coreAddons]
  )

  const rollback = useCallback(
    async (id: string) => {
      setRollingBackId(id)
      onError(null)
      try {
        onProposal(await rollbackManagementAddon(id), "rollback", id)
      } catch (err) {
        onError(errorMessage(err, `Failed to create rollback PR for ${id}`))
      } finally {
        setRollingBackId(null)
      }
    },
    [onProposal, onError]
  )

  const confirmRollback = useCallback(() => {
    const id = confirmRollbackId
    setConfirmRollbackId(null)
    if (id) void rollback(id)
  }, [confirmRollbackId, rollback])

  const onUpgradeProposal = useCallback(
    (proposal: Proposal) => {
      if (selectedAddon) onProposal(proposal, "upgrade", selectedAddon.id)
    },
    [selectedAddon, onProposal]
  )

  const onCatalogItemUpdated = useCallback((updated: CatalogItem) => {
    setCoreAddons((prev) =>
      prev.map((a) => (a.id === updated.id ? updated : a))
    )
    setSelectedAddon(updated)
  }, [])

  return {
    catalogById,
    rollingBackId,
    openUpgrade: setSelectedAddon,
    requestRollback: setConfirmRollbackId,
    selectedAddon,
    closeUpgrade: useCallback(() => setSelectedAddon(null), []),
    confirmRollbackId,
    cancelRollback: useCallback(() => setConfirmRollbackId(null), []),
    confirmRollback,
    onUpgradeProposal,
    onCatalogItemUpdated,
  }
}
