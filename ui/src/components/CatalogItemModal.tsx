import React, { useState } from "react"
import { Box, Loader2 } from "lucide-react"

import {
  CatalogItem,
  CreateCatalogItemPayload,
  TenantSummary,
} from "../types/api"
import { useAuthz } from "../lib/authz-context"
import { SchemaFieldDraft } from "../lib/schemaBuilder"
import { Button } from "./ui/button"
import { Checkbox } from "./ui/checkbox"
import { Input } from "./ui/input"
import { NativeSelect } from "./ui/native-select"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogFullscreenToggle,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog"
import { AppSourceFields } from "./catalog/AppSourceFields"
import { BundleItemsEditor } from "./catalog/BundleItemsEditor"
import { CheckboxRow, Field } from "./catalog/Field"
import { InfraBlueprintFields } from "./catalog/InfraBlueprintFields"
import { InlineAlert } from "./catalog/InlineAlert"
import { SchemaEditorSection } from "./catalog/SchemaEditorSection"
import {
  defaultBundleStep,
  isCatalogFormSubmittable,
} from "./catalog/catalogForm"

export interface CatalogItemModalProps {
  isOpen: boolean
  editingCatalogItem: CatalogItem | null
  catalog: CatalogItem[]
  appForm: CreateCatalogItemPayload
  setAppForm: React.Dispatch<React.SetStateAction<CreateCatalogItemPayload>>
  schemaJsonText: string
  setSchemaJsonText: React.Dispatch<React.SetStateAction<string>>
  schemaFields: SchemaFieldDraft[]
  setSchemaFields: React.Dispatch<React.SetStateAction<SchemaFieldDraft[]>>
  showSchemaEditor: boolean
  setShowSchemaEditor: React.Dispatch<React.SetStateAction<boolean>>
  appLoading: boolean
  appError: string | null
  tenants: TenantSummary[]
  onSubmit: (e: React.FormEvent) => void
  onExtractManifestVars: () => void
  onClose: () => void
  onCancel: () => void
}

const KIND_CATEGORY = {
  app: "Applications",
  infra: "Infrastructure",
  bundle: "Platform",
} as const

/** Add / edit a catalog entry (application, PKO blueprint, or bundle) via Git PR. */
export const CatalogItemModal: React.FC<CatalogItemModalProps> = (props) => {
  const {
    isOpen,
    editingCatalogItem,
    catalog,
    appForm,
    setAppForm,
    appLoading,
    appError,
    tenants,
    onSubmit,
    onExtractManifestVars,
    onClose,
    onCancel,
  } = props
  const authz = useAuthz()
  const [fullscreen, setFullscreen] = useState(false)
  const kind = appForm.kind || "app"
  const isEditing = Boolean(editingCatalogItem)

  // Mirror server rules: global entries need cluster admin, tenant entries need tenant admin.
  const scopeTenants = tenants.filter(
    (t) => authz.canAdminTenant(t.id) || t.id === appForm.tenant
  )
  const allowGlobal =
    authz.isAdmin || (isEditing && !editingCatalogItem?.tenant)

  const submitLabel = isEditing
    ? "Submit Update PR"
    : kind === "infra"
      ? "Submit Infra PR Proposal"
      : kind === "bundle"
        ? "Submit Bundle PR Proposal"
        : "Submit App PR Proposal"

  return (
    <Dialog
      open={isOpen}
      onOpenChange={(open) => {
        if (!open) {
          setFullscreen(false)
          onClose()
        }
      }}
    >
      <DialogContent
        size="xl"
        closeLabel="Close modal"
        data-testid="catalog-item-modal"
        className={
          fullscreen
            ? "sm:max-w-none w-screen h-dvh max-h-dvh rounded-none border-0"
            : "sm:max-h-[90vh]"
        }
        controls={
          <DialogFullscreenToggle
            fullscreen={fullscreen}
            onToggle={() => setFullscreen((prev) => !prev)}
          />
        }
      >
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Box className="h-4 w-4 shrink-0 text-indigo-400" />
            {isEditing ? "Edit Catalog Item" : "Add Catalog Item"}
          </DialogTitle>
          <DialogDescription>
            Changes are proposed as a Git pull request against the catalog.
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={onSubmit} className="flex min-h-0 flex-1 flex-col">
          <DialogBody className="space-y-4">
            <Field id="catalog-type" label="Catalog Type" required>
              <NativeSelect
                id="catalog-type"
                disabled={isEditing}
                value={kind}
                onChange={(e) => {
                  const newKind = e.target.value as "app" | "infra" | "bundle"
                  setAppForm((prev) => ({
                    ...prev,
                    kind: newKind,
                    category: KIND_CATEGORY[newKind],
                    items:
                      newKind === "bundle" &&
                      (!prev.items || prev.items.length === 0)
                        ? [defaultBundleStep(catalog)]
                        : prev.items,
                  }))
                }}
              >
                <option value="app">Application (Helm / Kustomize)</option>
                <option value="infra">Infrastructure Blueprint (PKO v2)</option>
                <option value="bundle">Multi-Item Bundle (Infra + Apps)</option>
              </NativeSelect>
            </Field>

            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <Field id="app-id" label="Item ID" required>
                <Input
                  id="app-id"
                  required
                  disabled={isEditing}
                  placeholder="e.g. podinfo"
                  value={appForm.id}
                  onChange={(e) =>
                    setAppForm((prev) => ({ ...prev, id: e.target.value }))
                  }
                />
              </Field>
              <Field id="app-name" label="Item Name" required>
                <Input
                  id="app-name"
                  required
                  className="font-sans"
                  placeholder={
                    kind === "infra"
                      ? "e.g. Proxmox Cluster Blueprint"
                      : kind === "bundle"
                        ? "e.g. kind Cluster + Podinfo Stack"
                        : "e.g. Podinfo Microservice"
                  }
                  value={appForm.name}
                  onChange={(e) =>
                    setAppForm((prev) => ({ ...prev, name: e.target.value }))
                  }
                />
              </Field>
            </div>

            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <Field id="catalog-tenant-scope" label="Tenant Visibility">
                <NativeSelect
                  id="catalog-tenant-scope"
                  value={appForm.tenant || ""}
                  onChange={(e) =>
                    setAppForm((prev) => ({ ...prev, tenant: e.target.value }))
                  }
                >
                  {allowGlobal && (
                    <option value="">All Tenants (Global)</option>
                  )}
                  {!allowGlobal && !appForm.tenant && (
                    <option value="" disabled>
                      Select a tenant…
                    </option>
                  )}
                  {scopeTenants.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.id}
                    </option>
                  ))}
                </NativeSelect>
              </Field>
              <div className="flex flex-col justify-end pb-2">
                <CheckboxRow
                  id="catalog-protected"
                  label="Protect Catalog Entry (Block Deletion)"
                >
                  <Checkbox
                    id="catalog-protected"
                    data-testid="catalog-protected-checkbox"
                    checked={Boolean(appForm.protected)}
                    onCheckedChange={(v) =>
                      setAppForm((prev) => ({ ...prev, protected: v === true }))
                    }
                  />
                </CheckboxRow>
              </div>
            </div>

            {kind === "app" && (
              <AppSourceFields
                appForm={appForm}
                setAppForm={setAppForm}
                onExtractManifestVars={onExtractManifestVars}
              />
            )}
            {kind === "infra" && (
              <InfraBlueprintFields
                appForm={appForm}
                setAppForm={setAppForm}
                isEditing={isEditing}
              />
            )}
            {kind === "bundle" && (
              <BundleItemsEditor
                appForm={appForm}
                setAppForm={setAppForm}
                catalog={catalog}
              />
            )}

            <SchemaEditorSection {...props} />

            {appError && <InlineAlert>{appError}</InlineAlert>}
          </DialogBody>

          <DialogFooter>
            <Button variant="secondary" onClick={onCancel}>
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={appLoading || !isCatalogFormSubmittable(appForm)}
            >
              {appLoading ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  <span>Opening PR...</span>
                </>
              ) : (
                <span>{submitLabel}</span>
              )}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
