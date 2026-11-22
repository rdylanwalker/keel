import React from "react"
import { Loader2, Plus } from "lucide-react"

import { CreateTenantResult, ProposalRef } from "../types/api"
import { Button } from "./ui/button"
import { Checkbox } from "./ui/checkbox"
import { Input } from "./ui/input"
import { Label } from "./ui/label"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog"
import { InlineAlert } from "./catalog/InlineAlert"
import { CheckboxRow } from "./catalog/Field"
import { TenantProvisioningSummary } from "./catalog/TenantProvisioningSummary"

export interface CreateTenantModalProps {
  isOpen: boolean
  newTenantId: string
  onNewTenantIdChange: (value: string) => void
  /** Adopt an existing tenant repository that already holds content (`adoptExisting`). */
  adoptExisting?: boolean
  onAdoptExistingChange?: (value: boolean) => void
  tenantLoading: boolean
  tenantError: string | null
  /** Provisioning result of the last successful submit (switches the dialog to its summary). */
  result?: CreateTenantResult | null
  onNavigateToPreview?: (ref: ProposalRef) => void
  onSubmit: (e: React.FormEvent) => void
  onClose: () => void
  onCancel: () => void
}

const TENANT_ID_PATTERN = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/

/** Steps `POST /api/v1/tenants` performs (shown before submitting). */
const PROVISIONING_STEPS = [
  "Creates the tenant's own private Git repository (<platform-repo>-tenant-<id>; in the keel-tenants organisation on Forgejo/Gitea, otherwise next to the platform repository) with a protected main branch",
  "Issues a read-only credential limited to that repository (HTTPS token, deploy key or deploy token, depending on the forge)",
  "Opens the platform PR adding the tenant-<id> namespace, RBAC, quotas and policies",
  "Once the namespace exists: writes the credentials Secret and a SOPS age key",
]

/** Admin-only dialog provisioning a new tenant (repository, credential, platform stub PR). */
export const CreateTenantModal: React.FC<CreateTenantModalProps> = ({
  isOpen,
  newTenantId,
  onNewTenantIdChange,
  adoptExisting = false,
  onAdoptExistingChange,
  tenantLoading,
  tenantError,
  result,
  onNavigateToPreview,
  onSubmit,
  onClose,
  onCancel,
}) => {
  const trimmed = newTenantId.trim()
  const invalid = trimmed !== "" && !TENANT_ID_PATTERN.test(trimmed)
  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        size="md"
        closeLabel="Close modal"
        data-testid="new-tenant-modal"
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Plus className="h-4 w-4 text-indigo-400" />
            {result ? "Tenant Provisioned" : "Create New Tenant"}
          </DialogTitle>
          <DialogDescription>
            {result
              ? "Merge the pull requests below to activate the tenant."
              : "Provisions a dedicated Git repository for the tenant and opens a pull request that adds its namespace, RBAC, and policies."}
          </DialogDescription>
        </DialogHeader>
        {result ? (
          <>
            <DialogBody>
              <TenantProvisioningSummary
                result={result}
                onNavigateToPreview={onNavigateToPreview}
              />
            </DialogBody>
            <DialogFooter>
              <Button onClick={onClose}>Done</Button>
            </DialogFooter>
          </>
        ) : (
          <form onSubmit={onSubmit} className="flex min-h-0 flex-1 flex-col">
            <DialogBody className="space-y-4">
              <div className="space-y-1">
                <Label htmlFor="new-tenant-id">Tenant ID</Label>
                <Input
                  id="new-tenant-id"
                  required
                  placeholder="e.g. acme"
                  value={newTenantId}
                  aria-invalid={invalid || undefined}
                  aria-describedby="new-tenant-id-help"
                  onChange={(e) => onNewTenantIdChange(e.target.value)}
                />
                <p
                  id="new-tenant-id-help"
                  className={`text-[11px] ${invalid ? "text-red-400" : "text-slate-500"}`}
                >
                  Lowercase letters, digits and &quot;-&quot; (DNS label).
                </p>
              </div>
              <ol
                aria-label="What creating a tenant does"
                className="list-decimal space-y-1 pl-5 text-[11px] text-slate-400"
              >
                {PROVISIONING_STEPS.map((step) => (
                  <li key={step}>{step}</li>
                ))}
              </ol>
              <p className="text-[11px] text-slate-500">
                With the development-only Local Git provider, tenants stay in
                the platform repository: no repository or credential is created.
              </p>
              {onAdoptExistingChange && (
                <div className="space-y-1">
                  <CheckboxRow
                    id="new-tenant-adopt"
                    label="Adopt an existing tenant repository"
                  >
                    <Checkbox
                      id="new-tenant-adopt"
                      checked={adoptExisting}
                      onCheckedChange={(v) => onAdoptExistingChange(v === true)}
                    />
                  </CheckboxRow>
                  <p className="text-[11px] text-slate-500">
                    Only when reusing the id of a deleted tenant whose
                    repository still holds content; otherwise such a repository
                    is refused.
                  </p>
                </div>
              )}
              {tenantError && <InlineAlert>{tenantError}</InlineAlert>}
            </DialogBody>
            <DialogFooter>
              <Button variant="secondary" onClick={onCancel}>
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={tenantLoading || !trimmed || invalid}
              >
                {tenantLoading ? (
                  <>
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    <span>Creating...</span>
                  </>
                ) : (
                  <span>Create Tenant PR</span>
                )}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}
