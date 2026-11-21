import React, { useEffect, useState } from "react"
import type { RJSFSchema } from "@rjsf/utils"
import {
  Search,
  Layers,
  CheckCircle2,
  AlertCircle,
  ArrowRight,
  GitPullRequest,
  Box,
  Plus,
  Trash2,
  X,
  Pencil,
} from "lucide-react"
import {
  CatalogItem,
  CreateCatalogItemPayload,
  Proposal,
  ReleaseSummary,
  StackSummary,
  TenantSummary,
} from "../types/api"
import {
  createCatalogApp,
  createTenant,
  deleteTenant,
  getCatalog,
  getReleaseConfig,
  getReleases,
  getStackConfig,
  getStacks,
  getTenants,
  updateCatalogItem,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"
import {
  DEFAULT_CATALOG,
  DEFAULT_STARTER_MANIFEST,
  DEMO_TENANTS,
} from "../lib/demoFixtures"
import {
  SchemaFieldDraft,
  extractManifestVariables,
  fieldsToSchemaJson,
  humanizeFieldName,
  schemaJsonToFields,
} from "../lib/schemaBuilder"
import {
  DeployInstanceInitialConfig,
  DeployInstanceModal,
} from "./DeployInstanceModal"
import { CreateTenantModal } from "./CreateTenantModal"
import { CatalogItemModal } from "./CatalogItemModal"

export interface CatalogWizardViewProps {
  onNavigateToPreview?: () => void
  initialReleaseTarget?: {
    namespace: string
    name: string
    kind?: "app" | "infra"
  } | null
  onClearReleaseTarget?: () => void
  selectedTenant?: string
}

export const CatalogWizardView: React.FC<CatalogWizardViewProps> = ({
  onNavigateToPreview,
  initialReleaseTarget,
  onClearReleaseTarget,
  selectedTenant,
}) => {
  const [catalog, setCatalog] = useState<CatalogItem[]>(() =>
    isDemoMode() ? DEFAULT_CATALOG : []
  )
  const [searchQuery, setSearchQuery] = useState("")
  const [deployModalItem, setDeployModalItem] = useState<CatalogItem | null>(
    null
  )
  const [deployInitialConfig, setDeployInitialConfig] =
    useState<DeployInstanceInitialConfig | null>(null)
  const [isDeployModalOpen, setIsDeployModalOpen] = useState(false)
  const [isDeployEditMode, setIsDeployEditMode] = useState(false)

  const [tenant, setTenant] = useState(selectedTenant || "default")
  const [prevSelectedTenant, setPrevSelectedTenant] = useState(selectedTenant)
  if (selectedTenant !== prevSelectedTenant) {
    setPrevSelectedTenant(selectedTenant)
    if (selectedTenant) {
      setTenant(selectedTenant)
    }
  }
  const [stacks, setStacks] = useState<StackSummary[]>([])
  const [releases, setReleases] = useState<ReleaseSummary[]>([])
  const [tenants, setTenants] = useState<TenantSummary[]>(() =>
    isDemoMode()
      ? DEMO_TENANTS
      : [{ id: "default", namespace: "tenant-default" }]
  )
  const [error, setError] = useState<string | null>(null)
  const [createdProposal, setCreatedProposal] = useState<Proposal | null>(null)

  const [isNewTenantModalOpen, setIsNewTenantModalOpen] = useState(false)
  const [newTenantId, setNewTenantId] = useState("")
  const [tenantLoading, setTenantLoading] = useState(false)
  const [tenantError, setTenantError] = useState<string | null>(null)

  const [isAddAppModalOpen, setIsAddAppModalOpen] = useState(false)
  const [editingCatalogItem, setEditingCatalogItem] =
    useState<CatalogItem | null>(null)
  const [schemaJsonText, setSchemaJsonText] = useState("")
  const [schemaFields, setSchemaFields] = useState<SchemaFieldDraft[]>([])
  const [showSchemaEditor, setShowSchemaEditor] = useState(false)
  const [appLoading, setAppLoading] = useState(false)
  const [appError, setAppError] = useState<string | null>(null)
  const [appForm, setAppForm] = useState<CreateCatalogItemPayload>({
    kind: "app",
    id: "",
    name: "",
    category: "Applications",
    runtime: "helm",
    repoUrl: "",
    chart: "",
    version: "",
    flaggerSupported: false,
    provider: "kind",
    pulumiRuntime: "yaml",
    entrypoint: "Pulumi.yaml",
    protectByDefault: false,
    terraformProvider: "",
    pulumiYaml: "",
    manifestsYaml: "",
    defaultEngine: "flux",
  })

  useEffect(() => {
    const fetchTenants = async () => {
      try {
        const items = await getTenants()
        if (items && items.length > 0) {
          setTenants(items)
          setTenant((prev) =>
            items.some((t) => t.id === prev) ? prev : items[0].id
          )
        } else {
          setTenants(
            isDemoMode()
              ? DEMO_TENANTS
              : [{ id: "default", namespace: "tenant-default" }]
          )
        }
      } catch {
        setTenants(
          isDemoMode()
            ? DEMO_TENANTS
            : [{ id: "default", namespace: "tenant-default" }]
        )
      }
    }
    fetchTenants()
    if (typeof getStacks === "function") {
      getStacks()
        .then((stackList) =>
          setStacks(
            (stackList || []).filter(
              (s) => s.name !== "management-cluster-core"
            )
          )
        )
        .catch(() => setStacks([]))
    }
    if (typeof getReleases === "function") {
      getReleases()
        .then((releaseList) => setReleases(releaseList || []))
        .catch(() => setReleases([]))
    }
  }, [])

  useEffect(() => {
    const loadCatalog = async () => {
      try {
        const items = await getCatalog()
        if (items && items.length > 0) {
          setCatalog(items)
        } else if (!isDemoMode()) {
          setCatalog([])
        }
      } catch (err) {
        const msg =
          err instanceof Error ? err.message : "Failed to load catalog"
        if (!(isDemoMode() && msg.includes("API error (401)"))) {
          setError(msg)
        }
        if (!isDemoMode()) {
          setCatalog([])
        }
      }
    }
    loadCatalog()
  }, [])

  useEffect(() => {
    if (!initialReleaseTarget || catalog.length === 0) return
    if (typeof getReleaseConfig !== "function") {
      onClearReleaseTarget?.()
      return
    }
    let cancelled = false
    const loadReleaseConfig = async () => {
      try {
        const isInfra = initialReleaseTarget.kind === "infra"
        const cfg = isInfra
          ? await getStackConfig(
              initialReleaseTarget.namespace,
              initialReleaseTarget.name
            )
          : await getReleaseConfig(
              initialReleaseTarget.namespace,
              initialReleaseTarget.name
            )
        if (cancelled) return
        const matched =
          catalog.find((item) => item.id === cfg.catalogId) ?? null
        setTenant(cfg.tenant || "default")
        setDeployModalItem(matched)
        setDeployInitialConfig({
          values: cfg.values,
          name: cfg.name || initialReleaseTarget.name,
          version: cfg.version || matched?.version || "",
          tenant: cfg.tenant || "default",
          targetCluster: cfg.targetCluster,
          targetNamespace: cfg.targetNamespace,
        })
        setIsDeployEditMode(true)
        setIsDeployModalOpen(Boolean(matched))
        setCreatedProposal(null)
        setError(null)
      } catch (err) {
        if (!cancelled) {
          setError(
            err instanceof Error
              ? err.message
              : "Failed to load release configuration"
          )
        }
      } finally {
        if (!cancelled) {
          onClearReleaseTarget?.()
        }
      }
    }
    loadReleaseConfig()
    return () => {
      cancelled = true
    }
  }, [initialReleaseTarget, catalog, onClearReleaseTarget])

  const openDeployModal = (item: CatalogItem) => {
    setDeployModalItem(item)
    setDeployInitialConfig(null)
    setIsDeployEditMode(false)
    setIsDeployModalOpen(true)
    setCreatedProposal(null)
    setError(null)
  }

  const filteredCatalog = catalog.filter(
    (item) =>
      item.name.toLowerCase().includes(searchQuery.toLowerCase()) ||
      item.description.toLowerCase().includes(searchQuery.toLowerCase()) ||
      item.category.toLowerCase().includes(searchQuery.toLowerCase())
  )

  const handleCreateTenant = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!newTenantId.trim()) return

    setTenantLoading(true)
    setTenantError(null)
    setCreatedProposal(null)

    try {
      const proposal = await createTenant(newTenantId.trim())
      setCreatedProposal(proposal)
      const newId = newTenantId.trim()
      setTenants((prev) =>
        prev.some((t) => t.id === newId)
          ? prev
          : [...prev, { id: newId, namespace: `tenant-${newId}` }]
      )
      setTenant(newId)
      setNewTenantId("")
      setIsNewTenantModalOpen(false)
    } catch (err) {
      setTenantError(
        err instanceof Error ? err.message : "Failed to create tenant proposal"
      )
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
      if (tenant === tenantId) {
        setTenant("default")
      }
    } catch (err) {
      setTenantError(
        err instanceof Error ? err.message : "Failed to remove tenant proposal"
      )
    } finally {
      setTenantLoading(false)
    }
  }

  const openAddCatalogModal = () => {
    setEditingCatalogItem(null)
    setAppError(null)
    setSchemaJsonText("")
    setSchemaFields([])
    setShowSchemaEditor(false)
    setAppForm({
      kind: "app",
      id: "",
      name: "",
      category: "Applications",
      description: "",
      runtime: "helm",
      repoUrl: "",
      chart: "",
      version: "",
      path: "",
      flaggerSupported: false,
      provider: "kind",
      pulumiRuntime: "yaml",
      entrypoint: "Pulumi.yaml",
      protectByDefault: false,
      terraformProvider: "",
      pulumiYaml: "",
      manifestsYaml: "",
      defaultEngine: "flux",
      items: [],
    })
    setIsAddAppModalOpen(true)
  }

  const openEditCatalogModal = (item: CatalogItem) => {
    const normalizedKind: "app" | "infra" | "bundle" =
      item.type === "infrastructure" || item.type === "infra"
        ? "infra"
        : item.type === "bundle"
          ? "bundle"
          : "app"
    setEditingCatalogItem(item)
    setAppError(null)
    const initialSchemaJson = JSON.stringify(
      item.schema ?? { type: "object", properties: {} },
      null,
      2
    )
    setSchemaJsonText(initialSchemaJson)
    setSchemaFields(schemaJsonToFields(initialSchemaJson) ?? [])
    setShowSchemaEditor(false)
    setAppForm({
      kind: normalizedKind,
      id: item.id,
      name: item.name,
      category:
        item.category ||
        (normalizedKind === "infra"
          ? "Infrastructure"
          : normalizedKind === "bundle"
            ? "Platform"
            : "Applications"),
      description: item.description || "",
      runtime: (item.runtime as "helm" | "kustomize" | "manifest") || "helm",
      manifestsYaml:
        item.manifestsYaml ||
        (item.runtime === "manifest" ? DEFAULT_STARTER_MANIFEST : ""),
      defaultEngine:
        item.defaultEngine ||
        (item.runtime === "manifest" ? "flux" : undefined),
      repoUrl: item.repoUrl || "",
      chart: item.chart || "",
      version: item.version || "",
      path: item.path || "",
      flaggerSupported: !!item.flaggerSupported,
      provider: item.provider || "kind",
      pulumiRuntime: item.pulumiRuntime || "yaml",
      entrypoint: item.entrypoint || "Pulumi.yaml",
      protectByDefault: !!item.protectByDefault,
      terraformProvider: "",
      pulumiYaml: item.pulumiYaml || "",
      items: item.bundleItems
        ? item.bundleItems.map((b) => ({
            ...b,
            dependsOn: b.dependsOn ? [...b.dependsOn] : [],
          }))
        : [],
    })
    setIsAddAppModalOpen(true)
  }

  const handleCreateCatalogApp = async (e: React.FormEvent) => {
    e.preventDefault()
    const isInfra = appForm.kind === "infra"
    const isBundle = appForm.kind === "bundle"
    const isManifest = appForm.kind === "app" && appForm.runtime === "manifest"
    if (
      !appForm.id.trim() ||
      !appForm.name.trim() ||
      (!isInfra && !isBundle && !isManifest && !appForm.repoUrl?.trim()) ||
      (isBundle && (!appForm.items || appForm.items.length === 0))
    )
      return

    let parsedSchema: RJSFSchema | undefined
    if (showSchemaEditor && schemaJsonText.trim()) {
      try {
        parsedSchema = JSON.parse(schemaJsonText) as RJSFSchema
      } catch (err) {
        setAppError(
          `Invalid JSON Schema: ${err instanceof Error ? err.message : "parse error"}`
        )
        return
      }
    }

    setAppLoading(true)
    setAppError(null)
    setCreatedProposal(null)

    try {
      const isEditing = Boolean(editingCatalogItem)
      const payload: CreateCatalogItemPayload = isBundle
        ? {
            kind: "bundle",
            id: appForm.id.trim(),
            name: appForm.name.trim(),
            category: appForm.category.trim() || "Platform",
            description: isEditing
              ? (appForm.description?.trim() ?? "")
              : appForm.description?.trim() || undefined,
            items: (appForm.items ?? []).map((item) => ({
              id: item.id.trim(),
              kind: item.kind,
              catalogRef: item.catalogRef.trim(),
              ...(item.dependsOn && item.dependsOn.length > 0
                ? { dependsOn: item.dependsOn }
                : {}),
              ...(item.kind === "app" && item.targetClusterFrom?.trim()
                ? { targetClusterFrom: item.targetClusterFrom.trim() }
                : {}),
              ...(item.kind === "app" && item.targetNamespace?.trim()
                ? { targetNamespace: item.targetNamespace.trim() }
                : {}),
              ...(item.kind === "app" && item.canary ? { canary: true } : {}),
            })),
            ...(parsedSchema ? { schema: parsedSchema } : {}),
          }
        : isInfra
          ? {
              kind: "infra",
              id: appForm.id.trim(),
              name: appForm.name.trim(),
              category: appForm.category.trim() || "Infrastructure",
              ...(isEditing
                ? { description: appForm.description?.trim() ?? "" }
                : appForm.description?.trim()
                  ? { description: appForm.description.trim() }
                  : {}),
              provider: appForm.provider || "kind",
              pulumiRuntime: appForm.pulumiRuntime || "yaml",
              entrypoint: appForm.entrypoint?.trim() || "Pulumi.yaml",
              terraformProvider: appForm.terraformProvider?.trim() || undefined,
              protectByDefault: !!appForm.protectByDefault,
              ...(appForm.pulumiYaml?.trim()
                ? { pulumiYaml: appForm.pulumiYaml }
                : {}),
              ...(parsedSchema ? { schema: parsedSchema } : {}),
            }
          : {
              kind: "app",
              id: appForm.id.trim(),
              name: appForm.name.trim(),
              category: appForm.category.trim() || "Applications",
              ...(isEditing
                ? { description: appForm.description?.trim() ?? "" }
                : appForm.description?.trim()
                  ? { description: appForm.description.trim() }
                  : {}),
              runtime: appForm.runtime || "helm",
              ...(appForm.runtime === "manifest"
                ? {
                    manifestsYaml:
                      appForm.manifestsYaml || DEFAULT_STARTER_MANIFEST,
                    defaultEngine: appForm.defaultEngine || "flux",
                    repoUrl: !appForm.repoUrl?.trim()
                      ? "https://kubernetes.io/docs/concepts/overview/working-with-objects/"
                      : appForm.repoUrl.trim(),
                    path: !appForm.path?.trim()
                      ? "./manifests.yaml"
                      : appForm.path.trim(),
                    chart: isEditing ? "" : undefined,
                    version: isEditing ? "" : undefined,
                  }
                : appForm.runtime === "kustomize"
                  ? {
                      repoUrl: appForm.repoUrl?.trim() || "",
                      path: isEditing
                        ? (appForm.path?.trim() ?? "")
                        : appForm.path?.trim() || undefined,
                      version: isEditing
                        ? (appForm.version?.trim() ?? "")
                        : appForm.version?.trim() || undefined,
                      chart: isEditing ? "" : undefined,
                    }
                  : {
                      repoUrl: appForm.repoUrl?.trim() || "",
                      chart: isEditing
                        ? (appForm.chart?.trim() ?? "")
                        : appForm.chart?.trim() || undefined,
                      version: isEditing
                        ? (appForm.version?.trim() ?? "")
                        : appForm.version?.trim() || undefined,
                      path: isEditing ? "" : undefined,
                    }),
              flaggerSupported: !!appForm.flaggerSupported,
              ...(parsedSchema ? { schema: parsedSchema } : {}),
            }

      const proposal = editingCatalogItem
        ? await updateCatalogItem(
            editingCatalogItem.type,
            editingCatalogItem.id,
            {
              name: payload.name,
              category: payload.category,
              description: payload.description,
              runtime: payload.runtime,
              manifestsYaml: payload.manifestsYaml,
              defaultEngine: payload.defaultEngine,
              repoUrl: payload.repoUrl,
              chart: payload.chart,
              version: payload.version,
              path: payload.path,
              flaggerSupported: payload.flaggerSupported,
              provider: payload.provider,
              pulumiRuntime: payload.pulumiRuntime,
              entrypoint: payload.entrypoint,
              protectByDefault: payload.protectByDefault,
              ...(payload.pulumiYaml ? { pulumiYaml: payload.pulumiYaml } : {}),
              items: payload.items,
              schema: payload.schema,
            }
          )
        : await createCatalogApp(payload)
      setCreatedProposal(proposal)
      setIsAddAppModalOpen(false)
      setEditingCatalogItem(null)
      setSchemaJsonText("")
      setShowSchemaEditor(false)
      setAppForm({
        kind: "app",
        id: "",
        name: "",
        category: "Applications",
        description: "",
        runtime: "helm",
        repoUrl: "",
        chart: "",
        version: "",
        path: "",
        flaggerSupported: false,
        provider: "kind",
        pulumiRuntime: "yaml",
        entrypoint: "Pulumi.yaml",
        protectByDefault: false,
        terraformProvider: "",
        pulumiYaml: "",
        manifestsYaml: "",
        defaultEngine: "flux",
        items: [],
      })
    } catch (err) {
      setAppError(
        err instanceof Error
          ? err.message
          : editingCatalogItem
            ? "Failed to update catalog item proposal"
            : "Failed to create catalog app proposal"
      )
    } finally {
      setAppLoading(false)
    }
  }

  const handleExtractManifestVars = () => {
    const yaml = appForm.manifestsYaml || DEFAULT_STARTER_MANIFEST
    const extracted = extractManifestVariables(yaml)
    const existingNames = new Set(schemaFields.map((f) => f.name))
    const newFields: SchemaFieldDraft[] = []

    for (const varName of extracted) {
      if (!existingNames.has(varName)) {
        const isSecret = /secret|token|password|key/i.test(varName)
        newFields.push({
          name: varName,
          title: humanizeFieldName(varName),
          type: "string",
          defaultValue: "",
          required: true,
          secret: isSecret,
        })
        existingNames.add(varName)
      }
    }

    const updated = [...schemaFields, ...newFields]
    setSchemaFields(updated)
    setSchemaJsonText(fieldsToSchemaJson(updated, schemaJsonText))
    setShowSchemaEditor(true)
  }

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 bg-slate-900 border border-slate-800 p-6 rounded-2xl">
        <div>
          <h2 className="text-xl font-bold text-white flex items-center gap-2">
            <Layers className="h-6 w-6 text-indigo-400" />
            Self-Service Catalog &amp; Dynamic Form Wizard
          </h2>
          <p className="text-xs text-slate-400 mt-1">
            Browse approved infrastructure and application blueprints, configure
            parameters, and propose changes via pull request
          </p>
        </div>

        <div className="flex items-center space-x-3">
          <label
            htmlFor="tenant-select-catalog"
            className="text-xs text-slate-400"
          >
            Target Tenant:
          </label>
          <select
            id="tenant-select-catalog"
            value={tenant}
            onChange={(e) => setTenant(e.target.value)}
            className="bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
          >
            {tenants.map((t) => (
              <option key={t.id} value={t.id}>
                {t.id}
              </option>
            ))}
          </select>

          <button
            type="button"
            onClick={() => handleDeleteTenant(tenant)}
            disabled={tenant === "default" || tenantLoading}
            title={
              tenant === "default"
                ? "The default tenant is protected and cannot be removed"
                : `Propose removal of tenant ${tenant}`
            }
            aria-label={
              tenant === "default"
                ? "Remove tenant (disabled for protected default tenant)"
                : `Remove tenant ${tenant}`
            }
            className="px-2.5 py-1.5 bg-red-950/60 hover:bg-red-900/80 disabled:hover:bg-red-950/60 border border-red-500/30 text-red-300 rounded-lg text-xs font-medium transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed flex items-center gap-1"
          >
            <Trash2 className="h-3.5 w-3.5" />
            <span>Remove</span>
          </button>

          <button
            type="button"
            onClick={() => setIsNewTenantModalOpen(true)}
            className="px-3 py-1.5 bg-slate-800 hover:bg-slate-700 text-indigo-300 rounded-lg text-xs font-semibold transition-colors border border-slate-700 flex items-center gap-1"
          >
            <Plus className="h-3.5 w-3.5" />
            <span>New Tenant</span>
          </button>
        </div>
      </div>

      {createdProposal && (
        <div
          aria-label="Proposal Floating Toast"
          className="fixed bottom-6 right-6 z-50 max-w-md bg-slate-900/95 backdrop-blur-md border border-emerald-500/40 shadow-2xl shadow-black/60 rounded-2xl p-4 text-slate-200 space-y-3"
        >
          <div className="flex items-start justify-between gap-3">
            <div className="flex items-start space-x-2.5 min-w-0">
              <CheckCircle2 className="h-5 w-5 text-emerald-400 shrink-0 mt-0.5" />
              <div className="min-w-0">
                <p className="text-xs font-bold text-white">
                  PR #{createdProposal.id} Opened
                </p>
                <p className="text-[11px] text-slate-400 truncate font-mono mt-0.5">
                  {createdProposal.branch}
                </p>
              </div>
            </div>
            <button
              type="button"
              onClick={() => setCreatedProposal(null)}
              aria-label="Dismiss notification"
              className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer shrink-0"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
          <div className="flex items-center justify-end gap-2">
            {createdProposal.prUrl && (
              <a
                href={createdProposal.prUrl}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center space-x-1 px-2.5 py-1.5 bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 rounded-lg text-xs font-medium transition-colors"
              >
                <span>View on Forge</span>
              </a>
            )}
            {onNavigateToPreview && (
              <button
                type="button"
                onClick={onNavigateToPreview}
                className="inline-flex items-center space-x-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 text-white rounded-lg text-xs font-semibold transition-colors cursor-pointer"
              >
                <span>Go to PR</span>
                <ArrowRight className="h-3.5 w-3.5" />
              </button>
            )}
          </div>
        </div>
      )}

      {createdProposal && (
        <div
          aria-label="Proposal Success Notification"
          className="bg-emerald-950/40 border border-emerald-500/30 p-5 rounded-2xl text-emerald-300 space-y-3 animate-in fade-in duration-300"
        >
          <div className="flex items-center space-x-3">
            <div className="p-2 bg-emerald-500/20 text-emerald-400 rounded-lg">
              <CheckCircle2 className="h-6 w-6" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-white">
                Git PR Proposal Successfully Opened!
              </h3>
              <p className="text-xs text-emerald-300">
                PR #{createdProposal.id} created on branch{" "}
                <code className="font-mono bg-slate-900 px-1 py-0.5 rounded">
                  {createdProposal.branch}
                </code>
              </p>
            </div>
          </div>
          <div className="text-xs bg-slate-900/90 border border-slate-800 rounded-xl p-4 flex flex-col sm:flex-row items-start sm:items-center justify-between gap-3 text-slate-300 font-mono">
            <div>
              <span className="text-slate-500">PR Title: </span>
              <span className="text-slate-100 font-semibold">
                {createdProposal.title}
              </span>
            </div>
            <div className="flex items-center space-x-2 shrink-0">
              {onNavigateToPreview && (
                <button
                  type="button"
                  onClick={onNavigateToPreview}
                  className="inline-flex items-center space-x-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 text-white rounded-lg font-sans text-xs font-medium transition-colors cursor-pointer"
                >
                  <span>Go to PR</span>
                  <ArrowRight className="h-3.5 w-3.5" />
                </button>
              )}
              {createdProposal.prUrl && (
                <a
                  href={createdProposal.prUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center space-x-1.5 px-3 py-1.5 bg-emerald-600 hover:bg-emerald-500 text-white rounded-lg font-sans text-xs font-medium transition-colors"
                >
                  <span>View PR on Forge</span>
                  <GitPullRequest className="h-3.5 w-3.5" />
                </a>
              )}
            </div>
          </div>
        </div>
      )}

      {/* Search & Add Catalog Item Bar */}
      <div className="flex flex-col sm:flex-row items-stretch sm:items-center justify-between gap-3">
        <div className="relative flex-1">
          <Search className="absolute left-3 top-2.5 h-4 w-4 text-slate-500" />
          <input
            type="text"
            placeholder="Search catalog blueprints..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            className="w-full pl-9 pr-4 py-2 bg-slate-900 border border-slate-800 rounded-xl text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
          />
        </div>
        <button
          type="button"
          onClick={openAddCatalogModal}
          className="px-3.5 py-2 bg-indigo-600 hover:bg-indigo-500 text-white rounded-xl text-xs font-semibold transition-colors shrink-0 flex items-center justify-center gap-1.5 shadow-md shadow-indigo-600/20 cursor-pointer"
        >
          <Plus className="h-4 w-4" />
          <span>Add Catalog Item</span>
        </button>
      </div>

      {error && (
        <div
          role="alert"
          className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-start space-x-2"
        >
          <AlertCircle className="h-4 w-4 text-red-400 shrink-0 mt-0.5" />
          <span>{error}</span>
        </div>
      )}

      {/* Catalog Grid */}
      <div
        data-testid="catalog-list"
        className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-3 gap-4"
      >
        {filteredCatalog.map((item) => (
          <div
            key={`${item.type}-${item.id}`}
            className="p-5 rounded-2xl border border-slate-800 bg-slate-900 flex flex-col justify-between gap-4 text-left"
          >
            <div className="space-y-2.5">
              <div className="flex items-start justify-between gap-2">
                <span className="text-sm font-bold text-white flex items-center gap-2">
                  <Box className="h-4 w-4 text-indigo-400 shrink-0" />
                  <span>{item.name}</span>
                </span>
                <span className="text-[10px] font-mono px-2 py-0.5 rounded bg-slate-800 text-slate-400 shrink-0">
                  {item.type === "bundle"
                    ? "bundle"
                    : `v${item.version.replace(/^v/i, "")}`}
                </span>
              </div>
              <p className="text-xs text-slate-400 line-clamp-2">
                {item.description}
              </p>
              <div className="flex items-center justify-between text-[10px] pt-1">
                <span className="px-2 py-0.5 rounded-full bg-slate-800 text-slate-300 font-medium">
                  {item.category}
                </span>
                <div className="flex items-center gap-2">
                  {item.provider && (
                    <span className="uppercase font-mono text-indigo-400">
                      {item.provider}
                    </span>
                  )}
                  <span className="font-mono text-slate-500">{item.id}</span>
                </div>
              </div>
            </div>

            <div className="flex items-center justify-end gap-2 pt-3 border-t border-slate-800/80">
              <button
                type="button"
                onClick={() => openEditCatalogModal(item)}
                className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer"
                title="Edit catalog item definition via Git PR"
              >
                <Pencil className="h-3.5 w-3.5" />
                <span>Edit</span>
              </button>
              <button
                type="button"
                onClick={() => openDeployModal(item)}
                className="inline-flex items-center gap-1.5 px-3.5 py-1.5 rounded-lg text-xs font-semibold bg-indigo-600 hover:bg-indigo-500 text-white transition-colors shadow-sm shadow-indigo-600/20 cursor-pointer"
              >
                <Plus className="h-3.5 w-3.5" />
                <span>Create</span>
              </button>
            </div>
          </div>
        ))}
        {filteredCatalog.length === 0 && (
          <div className="col-span-full p-6 rounded-2xl bg-slate-900 border border-slate-800 text-xs text-slate-500 text-center">
            {catalog.length === 0
              ? "No catalog blueprints available — check server configuration or authenticate."
              : `No catalog blueprints matching "${searchQuery}".`}
          </div>
        )}
      </div>

      {/* Deploy / Edit Instance Modal */}
      <DeployInstanceModal
        isOpen={isDeployModalOpen}
        onClose={() => {
          setIsDeployModalOpen(false)
          setDeployModalItem(null)
          setDeployInitialConfig(null)
          setIsDeployEditMode(false)
        }}
        catalogItem={deployModalItem}
        tenant={tenant}
        tenants={tenants}
        onTenantChange={setTenant}
        stacks={stacks}
        releases={releases}
        initialConfig={deployInitialConfig}
        isEditMode={isDeployEditMode}
        onProposalCreated={(proposal) => setCreatedProposal(proposal)}
        onCatalogItemUpdated={(updatedItem) => {
          setDeployModalItem(updatedItem)
          setCatalog((prev) =>
            prev.map((c) => (c.id === updatedItem.id ? updatedItem : c))
          )
        }}
      />

      {/* New Tenant Modal */}
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

      {/* Add Catalog Item Modal */}
      <CatalogItemModal
        isOpen={isAddAppModalOpen}
        editingCatalogItem={editingCatalogItem}
        catalog={catalog}
        appForm={appForm}
        setAppForm={setAppForm}
        schemaJsonText={schemaJsonText}
        setSchemaJsonText={setSchemaJsonText}
        schemaFields={schemaFields}
        setSchemaFields={setSchemaFields}
        showSchemaEditor={showSchemaEditor}
        setShowSchemaEditor={setShowSchemaEditor}
        appLoading={appLoading}
        appError={appError}
        onSubmit={handleCreateCatalogApp}
        onExtractManifestVars={handleExtractManifestVars}
        onClose={() => {
          setIsAddAppModalOpen(false)
          setEditingCatalogItem(null)
          setAppError(null)
        }}
        onCancel={() => {
          setIsAddAppModalOpen(false)
          setEditingCatalogItem(null)
          setAppError(null)
        }}
      />
    </div>
  )
}
