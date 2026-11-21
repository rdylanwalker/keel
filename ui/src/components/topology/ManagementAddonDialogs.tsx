import React from "react"

import { DeployInstanceModal } from "../DeployInstanceModal"
import { ConfirmDialog } from "../ui/confirm-dialog"
import { ManagementAddonActions } from "./useManagementAddonActions"

/**
 * Upgrade form (`catalog/core` schema via `@rjsf/core`) and rollback confirmation for the
 * management add-on rows of the releases table. Always mounted for admins, independent of the
 * table rows, so both survive a failed or empty releases refresh.
 */
export const ManagementAddonDialogs: React.FC<{
  actions: ManagementAddonActions
}> = ({ actions }) => (
  <>
    <ConfirmDialog
      open={actions.confirmRollbackId !== null}
      onOpenChange={(open) => {
        if (!open) actions.cancelRollback()
      }}
      title={`Roll back ${actions.confirmRollbackId ?? ""}?`}
      description="This opens a pull request reverting the management add-on manifest to its previous Git revision. Nothing changes until the PR is reviewed and merged."
      confirmLabel="Open Rollback PR"
      onConfirm={actions.confirmRollback}
    />

    <DeployInstanceModal
      isOpen={Boolean(actions.selectedAddon)}
      onClose={actions.closeUpgrade}
      catalogItem={actions.selectedAddon}
      managementAddon={actions.selectedAddon}
      tenant="keel-system"
      onProposalCreated={actions.onUpgradeProposal}
      onCatalogItemUpdated={actions.onCatalogItemUpdated}
    />
  </>
)
