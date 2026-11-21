import React, { useEffect, useState } from "react"
import { Filter, Layers, Plus, Search, Trash2, X } from "lucide-react"

import { CatalogItem, Proposal } from "../types/api"
import {
  createTenant,
  deleteCatalogItem,
  deleteTenant,
  getReleaseConfig,
  getStackConfig,
} from "../lib/api"
import { useAuthz } from "../lib/authz-context"
import { errorMessage } from "../lib/errors"
import type { CatalogKindFilter } from "../lib/routes"
import { Button } from "./ui/button"
import { PageHeader } from "./ui/page-header"
import { Input } from "./ui/input"
import { NativeSelect } from "./ui/native-select"
import {
  DeployInstanceInitialConfig,
  DeployInstanceModal,
} from "./DeployInstanceModal"
import { CreateTenantModal } from "./CreateTenantModal"
import { CatalogItemModal } from "./CatalogItemModal"
import { CatalogItemCard } from "./catalog/CatalogItemCard"
import { InlineAlert } from "./catalog/InlineAlert"
import { ProposalCreatedNotice } from "./catalog/ProposalCreatedNotice"
import { KIND_FILTER_LABELS, matchesKindFilter } from "./catalog/kindFilter"
import { useCatalogData } from "./catalog/useCatalogData"
import { useCatalogItemEditor } from "./catalog/useCatalogItemEditor"

export interface CatalogWizardViewProps {
  onNavigateToPreview?: () => void
  initialReleaseTarget?: {
    namespace: string
    name: string
    kind?: "app" | "infra"
  } | null
  onClearReleaseTarget?: () => void
  selectedTenant?: string
  /** Pre-filter from `#catalog/<kind>` ("+ Deploy Cluster/VM/Service/App"). */
  kindFilter?: CatalogKindFilter
  onKindFilterChange?: (kind?: CatalogKindFilter) => void
}

interface DeployModalState {
  item: CatalogItem | null
  initialConfig: DeployInstanceInitialConfig | null
  editMode: boolean
}

export const CatalogWizardView: React.FC<CatalogWizardViewProps> = ({
  onNavigateToPreview,
  initialReleaseTarget,
  onClearReleaseTarget,
  selectedTenant,
  kindFilter,
  onKindFilterChange,
}) => {
  const authz = useAuthz()
  const [searchQuery, setSearchQuery] = useState("")
  const [deletingCatalogId, setDeletingCatalogId] = useState<string | null>(
    null
  )
  const [deployModal, setDeployModal] = useState<DeployModalState | null>(null)
  const [activeKindFilter, setActiveKindFilter] = useState(kindFilter)
  const [prevKindFilter, setPrevKindFilter] = useState(kindFilter)
  if (kindFilter !== prevKindFilter) {
    setPrevKindFilter(kindFilter)
    setActiveKindFilter(kindFilter)
  }

  // `tenant` is the deploy target; `tenantFilter` narrows the visible catalog.
  const [tenant, setTenantState] = useState(selectedTenant || "default")
  const [tenantFilter, setTenantFilter] = useState(selectedTenant || "all")
  const setTenant = (next: string) => {
    setTenantState(next)
    setTenantFilter(next)
  }
  const [prevSelectedTenant, setPrevSelectedTenant] = useState(selectedTenant)
  if (selectedTenant !== prevSelectedTenant) {
    setPrevSelectedTenant(selectedTenant)
    setTenant(
      selectedTenant && selectedTenant !== "all" ? selectedTenant : "all"
    )
  }
  const [error, setError] = useState<string | null>(null)
  const [createdProposal, setCreatedProposal] = useState<Proposal | null>(null)

  const [isNewTenantModalOpen, setIsNewTenantModalOpen] = useState(false)
  const [newTenantId, setNewTenantId] = useState("")
  const [tenantLoading, setTenantLoading] = useState(false)
  const [tenantError, setTenantError] = useState<string | null>(null)

  const {
    catalog,
    setCatalog,
    tenants,
    setTenants,
    stacks,
    releases,
    loadError,
  } = useCatalogData((items) =>
    setTenantState((prev) =>
      prev === "all" || items.some((t) => t.id === prev) ? prev : items[0].id
    )
  )
  const editor = useCatalogItemEditor(setCreatedProposal)

  useEffect(() => {
    if (!initialReleaseTarget || catalog.length === 0) return
    let cancelled = false
    const target = initialReleaseTarget
    const load = async () => {
      try {
        const cfg =
          target.kind === "infra"
            ? await getStackConfig(target.namespace, target.name)
            : await getReleaseConfig(target.namespace, target.name)
        if (cancelled) return
        const matched =
          catalog.find((item) => item.id === cfg.catalogId) ?? null
        setTenant(cfg.tenant || "default")
        setDeployModal(
          matched
            ? {
                item: matched,
                editMode: true,
                initialConfig: {
                  values: cfg.values,
                  name: cfg.name || target.name,
                  version: cfg.version || matched.version || "",
                  tenant: cfg.tenant || "default",
                  targetCluster: cfg.targetCluster,
                  targetNamespace: cfg.targetNamespace,
                },
              }
            : null
        )
        setCreatedProposal(null)
        setError(null)
      } catch (err) {
        if (!cancelled) {
          setError(errorMessage(err, "Failed to load release configuration"))
        }
      } finally {
        if (!cancelled) onClearReleaseTarget?.()
      }
    }
    void load()
    return () => {
      cancelled = true
    }
  }, [initialReleaseTarget, catalog, onClearReleaseTarget])

  const canManageItem = (item: CatalogItem) =>
    item.tenant ? authz.canAdminTenant(item.tenant) : authz.isAdmin
  const canAddCatalogItem =
    authz.isAdmin || tenants.some((t) => authz.canAdminTenant(t.id))

  const query = searchQuery.toLowerCase()
  const filteredCatalog = catalog.filter((item) => {
    if (
      tenantFilter &&
      tenantFilter !== "all" &&
      item.tenant &&
      item.tenant !== tenantFilter
    ) {
      return false
    }
    if (!matchesKindFilter(item, activeKindFilter)) return false
    return (
      item.name.toLowerCase().includes(query) ||
      item.description.toLowerCase().includes(query) ||
      item.category.toLowerCase().includes(query)
    )
  })

  const clearKindFilter = () => {
    setActiveKindFilter(undefined)
    onKindFilterChange?.(undefined)
  }

  const handleCreateTenant = async (e: React.FormEvent) => {
    e.preventDefault()
    const newId = newTenantId.trim()
    if (!newId) return
    setTenantLoading(true)
    setTenantError(null)
    setCreatedProposal(null)
    try {
      const proposal = await createTenant(newId)
      setCreatedProposal(proposal)
      setTenants((prev) =>
        prev.some((t) => t.id === newId)
          ? prev
          : [...prev, { id: newId, namespace: `tenant-${newId}` }]
      )
      setTenant(newId)
      setNewTenantId("")
      setIsNewTenantModalOpen(false)
    } catch (err) {
      setTenantError(errorMessage(err, "Failed to create tenant proposal"))
    } finally {
      setTenantLoading(false)
    }
  }

  const handleDeleteTenant = async (tenantId: string) => {
    if (tenantId === "default") return
    setTenantLoading(true)
    setTenantError(null)
    setCreatedProposal(null)
    try {
      const proposal = await deleteTenant(tenantId)
      setCreatedProposal(proposal)
      setTenants((prev) => prev.filter((t) => t.id !== tenantId))
      if (tenant === tenantId) setTenant("default")
    } catch (err) {
      setTenantError(errorMessage(err, "Failed to remove tenant proposal"))
    } finally {
      setTenantLoading(false)
    }
  }

  const handleDeleteCatalogItem = async (item: CatalogItem) => {
    if (item.protected || deletingCatalogId === item.id) return
    setDeletingCatalogId(item.id)
    try {
      const proposal = await deleteCatalogItem(item.type, item.id)
      setCreatedProposal(proposal)
      setCatalog((prev) => prev.filter((c) => c.id !== item.id))
    } catch (err) {
      setError(errorMessage(err, "Failed to delete catalog item"))
    } finally {
      setDeletingCatalogId(null)
    }
  }

  const tenantIsProtected = tenant === "default" || tenant === "all" || !tenant
  const shownError = error ?? loadError

  return (
    <div className="space-y-6">
      <PageHeader
        inlineAt="4xl"
        icon={<Layers className="h-6 w-6 text-indigo-400" />}
        title="Self-Service Catalog & Dynamic Form Wizard"
        description="Browse approved infrastructure and application blueprints, configure parameters, and propose changes via pull request"
        actions={
          <>
            <div className="flex min-w-0 items-center gap-2">
              <label
                htmlFor="tenant-select-catalog"
                className="shrink-0 whitespace-nowrap text-xs text-slate-400"
              >
                Target Tenant:
              </label>
              <div className="w-36 min-w-0">
                <NativeSelect
                  id="tenant-select-catalog"
                  value={tenant}
                  onChange={(e) => setTenant(e.target.value)}
                  className="h-8 py-1"
                >
                  <option value="all">All Tenants</option>
                  {tenants.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.id}
                    </option>
                  ))}
                </NativeSelect>
              </div>
            </div>
            {authz.isAdmin && (
              <div className="flex items-center gap-2">
                <Button
                  variant="destructive-subtle"
                  onClick={() => handleDeleteTenant(tenant)}
                  disabled={tenantIsProtected || tenantLoading}
                  title={
                    tenantIsProtected
                      ? "The default tenant is protected and cannot be removed"
                      : `Propose removal of tenant ${tenant}`
                  }
                  aria-label={
                    tenantIsProtected
                      ? "Remove tenant (disabled for protected default tenant)"
                      : `Remove tenant ${tenant}`
                  }
                >
                  <Trash2 className="h-3.5 w-3.5" />
                  <span>Remove</span>
                </Button>
                <Button
                  variant="secondary"
                  className="text-indigo-300"
                  onClick={() => setIsNewTenantModalOpen(true)}
                >
                  <Plus className="h-3.5 w-3.5" />
                  <span>New Tenant</span>
                </Button>
              </div>
            )}
          </>
        }
      />

      {tenantError && !isNewTenantModalOpen && (
        <InlineAlert>{tenantError}</InlineAlert>
      )}

      {createdProposal && (
        <ProposalCreatedNotice
          proposal={createdProposal}
          onDismiss={() => setCreatedProposal(null)}
          onNavigateToPreview={onNavigateToPreview}
        />
      )}

      <div className="flex flex-col sm:flex-row items-stretch sm:items-center justify-between gap-3">
        <div className="relative flex-1">
          <Search
            className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-slate-500"
            aria-hidden="true"
          />
          <Input
            type="search"
            aria-label="Search catalog blueprints"
            placeholder="Search catalog blueprints..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            className="pl-9 rounded-xl bg-slate-900 font-sans"
          />
        </div>
        {canAddCatalogItem && (
          <Button
            size="lg"
            className="rounded-xl"
            onClick={() =>
              editor.openCreate(
                authz.isAdmin
                  ? undefined
                  : {
                      tenant:
                        tenants.find((t) => authz.canAdminTenant(t.id))?.id ??
                        "",
                    }
              )
            }
          >
            <Plus className="h-4 w-4" />
            <span>Add Catalog Item</span>
          </Button>
        )}
      </div>

      {activeKindFilter && (
        <div
          className="flex flex-wrap items-center gap-2 text-xs text-slate-400"
          data-testid="catalog-kind-filter"
        >
          <Filter className="h-3.5 w-3.5 text-indigo-400" aria-hidden="true" />
          <span>Showing:</span>
          <span className="inline-flex items-center gap-1 rounded-full border border-indigo-500/40 bg-indigo-500/10 pl-2.5 pr-1 py-0.5 text-indigo-200">
            {KIND_FILTER_LABELS[activeKindFilter]}
            <button
              type="button"
              onClick={clearKindFilter}
              aria-label="Clear catalog kind filter"
              className="rounded-full p-0.5 hover:bg-indigo-500/20 cursor-pointer"
            >
              <X className="h-3 w-3" />
            </button>
          </span>
        </div>
      )}

      {shownError && <InlineAlert>{shownError}</InlineAlert>}

      <div
        data-testid="catalog-list"
        className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4"
      >
        {filteredCatalog.map((item) => (
          <CatalogItemCard
            key={`${item.type}-${item.id}`}
            item={item}
            canManage={canManageItem(item)}
            deleting={deletingCatalogId === item.id}
            onDelete={() => handleDeleteCatalogItem(item)}
            onEdit={() => editor.openEdit(item)}
            onCreate={() => {
              setDeployModal({ item, initialConfig: null, editMode: false })
              setCreatedProposal(null)
              setError(null)
            }}
          />
        ))}
        {filteredCatalog.length === 0 && (
          <div className="col-span-full p-6 rounded-2xl bg-slate-900 border border-slate-800 text-xs text-slate-500 text-center">
            {catalog.length === 0
              ? "No catalog blueprints available — check server configuration or authenticate."
              : activeKindFilter && !searchQuery
                ? `No ${KIND_FILTER_LABELS[activeKindFilter].toLowerCase()} blueprints in the catalog.`
                : `No catalog blueprints matching "${searchQuery}".`}
          </div>
        )}
      </div>

      <DeployInstanceModal
        isOpen={Boolean(deployModal?.item)}
        onClose={() => setDeployModal(null)}
        catalogItem={deployModal?.item ?? null}
        tenant={
          deployModal?.item?.tenant ||
          (tenant && tenant !== "all" ? tenant : tenants[0]?.id || "default")
        }
        tenants={tenants}
        onTenantChange={setTenant}
        stacks={stacks}
        releases={releases}
        initialConfig={deployModal?.initialConfig}
        isEditMode={deployModal?.editMode ?? false}
        onProposalCreated={setCreatedProposal}
        onCatalogItemUpdated={(updated) => {
          setDeployModal((prev) => (prev ? { ...prev, item: updated } : prev))
          setCatalog((prev) =>
            prev.map((c) => (c.id === updated.id ? updated : c))
          )
        }}
      />

      {authz.isAdmin && (
        <CreateTenantModal
          isOpen={isNewTenantModalOpen}
          newTenantId={newTenantId}
          onNewTenantIdChange={setNewTenantId}
          tenantLoading={tenantLoading}
          tenantError={tenantError}
          onSubmit={handleCreateTenant}
          onClose={() => {
            setIsNewTenantModalOpen(false)
            setTenantError(null)
          }}
          onCancel={() => {
            setIsNewTenantModalOpen(false)
            setTenantError(null)
          }}
        />
      )}

      <CatalogItemModal
        {...editor.modalProps}
        catalog={catalog}
        tenants={tenants}
      />
    </div>
  )
}
