import React from "react"
import { Loader2, Plus } from "lucide-react"

import { Button } from "./ui/button"
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

export interface CreateTenantModalProps {
  isOpen: boolean
  newTenantId: string
  onNewTenantIdChange: (value: string) => void
  tenantLoading: boolean
  tenantError: string | null
  onSubmit: (e: React.FormEvent) => void
  onClose: () => void
  onCancel: () => void
}

const TENANT_ID_PATTERN = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/

/** Admin-only dialog proposing a new tenant namespace via Git PR. */
export const CreateTenantModal: React.FC<CreateTenantModalProps> = ({
  isOpen,
  newTenantId,
  onNewTenantIdChange,
  tenantLoading,
  tenantError,
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
            Create New Tenant
          </DialogTitle>
          <DialogDescription>
            Opens a pull request that provisions the{" "}
            <code className="font-mono">tenant-&lt;id&gt;</code> namespace,
            RBAC, and policies.
          </DialogDescription>
        </DialogHeader>
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
      </DialogContent>
    </Dialog>
  )
}
