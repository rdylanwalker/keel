import React, { useEffect, useState } from "react"
import Form from "@rjsf/core"
import validator from "@rjsf/validator-ajv8"
import { IconButtonProps, RJSFSchema, ValidatorType } from "@rjsf/utils"
import {
  Search,
  Layers,
  FileCode2,
  CheckCircle2,
  AlertCircle,
  Loader2,
  ArrowRight,
  ArrowUp,
  ArrowDown,
  Copy,
  GitPullRequest,
  Box,
  Plus,
  Trash2,
  X,
  RefreshCw,
  AlertTriangle,
  Pencil,
} from "lucide-react"
import {
  CatalogBundleItem,
  CatalogItem,
  CreateCatalogItemPayload,
  Proposal,
  ReleaseSummary,
  StackSummary,
  TenantSummary,
} from "../types/api"
import {
  createCatalogApp,
  createProposal,
  createTenant,
  deleteTenant,
  fetchCatalogSchema,
  getCatalog,
  getReleaseConfig,
  getReleases,
  getStackConfig,
  getStacks,
  getTenants,
  updateCatalogItem,
} from "../lib/api"
import { jsonToYaml, isDemoMode } from "../lib/utils"

const DEMO_TENANTS: TenantSummary[] = [
  { id: "default", namespace: "tenant-default" },
  { id: "acme", namespace: "tenant-acme" },
  { id: "globex", namespace: "tenant-globex" },
  { id: "stark", namespace: "tenant-stark" },
]

const DEFAULT_CATALOG: CatalogItem[] = [
  {
    id: "app-web-service",
    name: "Web Service Application",
    description:
      "Standard HTTP microservice Helm chart with Envoy ingress and HPA",
    type: "app",
    category: "Applications",
    version: "1.4.0",
    schema: {
      title: "Web Service Blueprint",
      type: "object",
      required: ["appName", "replicas", "image"],
      properties: {
        appName: {
          type: "string",
          title: "Application Name",
          default: "frontend-portal",
        },
        replicas: {
          type: "number",
          title: "Replica Count",
          default: 3,
          minimum: 1,
        },
        image: {
          type: "string",
          title: "Container Image",
          default: "ghcr.io/acme/portal:v1.2.0",
        },
        ingressHost: {
          type: "string",
          title: "Ingress Hostname",
          default: "portal.acme.internal",
        },
        enableCanary: {
          type: "boolean",
          title: "Enable Flagger Progressive Canary Rollouts",
          default: true,
        },
      },
    },
    defaultValues: {
      appName: "frontend-portal",
      replicas: 3,
      image: "ghcr.io/acme/portal:v1.2.0",
      ingressHost: "portal.acme.internal",
      enableCanary: true,
    },
  },
  {
    id: "infra-proxmox-vm",
    name: "Proxmox VM Cluster Stack",
    description:
      "Proxmox VE virtual machine pool synthesized via PKO v2 OpenTofu/Pulumi provider",
    type: "infrastructure",
    category: "Infrastructure",
    version: "2.1.0",
    provider: "proxmox",
    schema: {
      title: "Proxmox VM Pool Blueprint",
      type: "object",
      required: ["poolName", "nodeCount", "cores", "memoryMb"],
      properties: {
        poolName: {
          type: "string",
          title: "VM Pool Name",
          default: "k8s-worker-pool",
        },
        nodeCount: {
          type: "number",
          title: "Node Count",
          default: 3,
          minimum: 1,
        },
        cores: {
          type: "number",
          title: "CPU Cores per Node",
          default: 4,
          minimum: 1,
        },
        memoryMb: {
          type: "number",
          title: "RAM (MB) per Node",
          default: 8192,
          minimum: 1024,
        },
        storagePool: {
          type: "string",
          title: "Proxmox Storage Target",
          default: "local-zfs",
        },
      },
    },
    defaultValues: {
      poolName: "k8s-worker-pool",
      nodeCount: 3,
      cores: 4,
      memoryMb: 8192,
      storagePool: "local-zfs",
    },
  },
  {
    id: "app-postgres-db",
    name: "PostgreSQL Database Cluster",
    description:
      "HA PostgreSQL database with SOPS age encrypted credentials and backup schedule",
    type: "app",
    category: "Databases",
    version: "15.4.0",
    schema: {
      title: "PostgreSQL Database Blueprint",
      type: "object",
      required: ["dbName", "storageGb", "adminPassword"],
      properties: {
        dbName: {
          type: "string",
          title: "Database Name",
          default: "tenant_db",
        },
        storageGb: {
          type: "number",
          title: "Storage Size (GB)",
          default: 50,
          minimum: 5,
        },
        adminPassword: {
          type: "string",
          title: "Admin Password (Encrypted with SOPS + age)",
          format: "password",
          default: "SecretP@ssw0rd!",
        },
        highAvailability: {
          type: "boolean",
          title: "Enable HA Replication",
          default: true,
        },
      },
    },
    defaultValues: {
      dbName: "tenant_db",
      storageGb: 50,
      adminPassword: "SecretP@ssw0rd!",
      highAvailability: true,
    },
  },
]

function extractSchemaDefaults(
  schema?: Record<string, unknown>,
  defaultValues?: Record<string, unknown>
): Record<string, unknown> {
  const result: Record<string, unknown> = { ...(defaultValues ?? {}) }
  const properties = schema?.properties
  if (
    !properties ||
    typeof properties !== "object" ||
    Array.isArray(properties)
  ) {
    return result
  }
  for (const [key, rawProp] of Object.entries(
    properties as Record<string, unknown>
  )) {
    if (!rawProp || typeof rawProp !== "object" || Array.isArray(rawProp)) {
      continue
    }
    const prop = rawProp as Record<string, unknown>
    if (result[key] === undefined && prop.default !== undefined) {
      result[key] = prop.default
    } else if (
      prop.type === "object" &&
      prop.properties &&
      typeof prop.properties === "object"
    ) {
      const nested = extractSchemaDefaults(
        prop,
        result[key] && typeof result[key] === "object"
          ? (result[key] as Record<string, unknown>)
          : undefined
      )
      if (Object.keys(nested).length > 0) {
        result[key] = nested
      }
    }
  }
  return result
}

export interface CatalogWizardViewProps {
  onNavigateToPreview?: () => void
  initialReleaseTarget?: {
    namespace: string
    name: string
    kind?: "app" | "infra"
  } | null
  onClearReleaseTarget?: () => void
}

const RjsfAddButton: React.FC<IconButtonProps> = ({
  onClick,
  disabled,
  className,
}) => (
  <button
    type="button"
    onClick={onClick}
    disabled={disabled}
    className={`inline-flex items-center gap-1.5 px-2.5 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer disabled:opacity-50 ${className ?? ""}`}
    title="Add item"
  >
    <Plus className="h-3.5 w-3.5" />
    <span>Add</span>
  </button>
)

const RjsfRemoveButton: React.FC<IconButtonProps> = ({
  onClick,
  disabled,
  className,
}) => (
  <button
    type="button"
    onClick={onClick}
    disabled={disabled}
    className={`inline-flex items-center justify-center p-1.5 rounded-lg text-xs bg-red-500/10 hover:bg-red-500/20 text-red-400 border border-red-500/25 transition-colors cursor-pointer disabled:opacity-50 ${className ?? ""}`}
    title="Remove item"
    aria-label="Remove item"
  >
    <Trash2 className="h-3.5 w-3.5" />
  </button>
)

const RjsfMoveUpButton: React.FC<IconButtonProps> = ({
  onClick,
  disabled,
  className,
}) => (
  <button
    type="button"
    onClick={onClick}
    disabled={disabled}
    className={`inline-flex items-center justify-center p-1.5 rounded-lg text-xs bg-slate-800 hover:bg-slate-700 text-slate-300 border border-slate-700 transition-colors cursor-pointer disabled:opacity-40 ${className ?? ""}`}
    title="Move up"
    aria-label="Move up"
  >
    <ArrowUp className="h-3.5 w-3.5" />
  </button>
)

const RjsfMoveDownButton: React.FC<IconButtonProps> = ({
  onClick,
  disabled,
  className,
}) => (
  <button
    type="button"
    onClick={onClick}
    disabled={disabled}
    className={`inline-flex items-center justify-center p-1.5 rounded-lg text-xs bg-slate-800 hover:bg-slate-700 text-slate-300 border border-slate-700 transition-colors cursor-pointer disabled:opacity-40 ${className ?? ""}`}
    title="Move down"
    aria-label="Move down"
  >
    <ArrowDown className="h-3.5 w-3.5" />
  </button>
)

const RjsfCopyButton: React.FC<IconButtonProps> = ({
  onClick,
  disabled,
  className,
}) => (
  <button
    type="button"
    onClick={onClick}
    disabled={disabled}
    className={`inline-flex items-center justify-center p-1.5 rounded-lg text-xs bg-slate-800 hover:bg-slate-700 text-slate-300 border border-slate-700 transition-colors cursor-pointer disabled:opacity-40 ${className ?? ""}`}
    title="Duplicate item"
    aria-label="Duplicate item"
  >
    <Copy className="h-3.5 w-3.5" />
  </button>
)

const RJSF_BUTTON_TEMPLATES = {
  AddButton: RjsfAddButton,
  RemoveButton: RjsfRemoveButton,
  MoveUpButton: RjsfMoveUpButton,
  MoveDownButton: RjsfMoveDownButton,
  CopyButton: RjsfCopyButton,
}

export const CatalogWizardView: React.FC<CatalogWizardViewProps> = ({
  onNavigateToPreview,
  initialReleaseTarget,
  onClearReleaseTarget,
}) => {
  const [catalog, setCatalog] = useState<CatalogItem[]>(() =>
    isDemoMode() ? DEFAULT_CATALOG : []
  )
  const [searchQuery, setSearchQuery] = useState("")
  const [selectedItem, setSelectedItem] = useState<CatalogItem | null>(() =>
    isDemoMode() ? DEFAULT_CATALOG[0] : null
  )
  const [formData, setFormData] = useState<Record<string, unknown>>(() =>
    isDemoMode()
      ? (DEFAULT_CATALOG[0].defaultValues as Record<string, unknown>) || {}
      : {}
  )
  const [tenant, setTenant] = useState("default")
  const [stacks, setStacks] = useState<StackSummary[]>([])
  const [releases, setReleases] = useState<ReleaseSummary[]>([])
  const [targetCluster, setTargetCluster] = useState("")
  const [customClusterSecret, setCustomClusterSecret] = useState("")
  const [instanceName, setInstanceName] = useState<string>(() =>
    isDemoMode() ? DEFAULT_CATALOG[0].id : ""
  )
  const [chartVersion, setChartVersion] = useState<string>(() =>
    isDemoMode() ? DEFAULT_CATALOG[0].version || "" : ""
  )
  const [targetNamespace, setTargetNamespace] = useState<string>("")
  const [tenants, setTenants] = useState<TenantSummary[]>(() =>
    isDemoMode()
      ? DEMO_TENANTS
      : [{ id: "default", namespace: "tenant-default" }]
  )
  const [loading, setLoading] = useState(false)
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
  const [showSchemaEditor, setShowSchemaEditor] = useState(false)
  const [appLoading, setAppLoading] = useState(false)
  const [syncingSchema, setSyncingSchema] = useState(false)
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
          setSelectedItem((prev) => {
            if (prev && items.some((i) => i.id === prev.id)) return prev
            return items[0]
          })
          setInstanceName((prev) => prev || items[0].id)
          setChartVersion((prev) => prev || items[0].version || "")
          setFormData((prev) => {
            if (Object.keys(prev).length > 0) return prev
            return extractSchemaDefaults(
              items[0].schema as Record<string, unknown>,
              items[0].defaultValues as Record<string, unknown> | undefined
            )
          })
        } else if (!isDemoMode()) {
          setCatalog([])
          setSelectedItem(null)
          setFormData({})
        }
      } catch (err) {
        setError(err instanceof Error ? err.message : "Failed to load catalog")
        if (!isDemoMode()) {
          setCatalog([])
          setSelectedItem(null)
          setFormData({})
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
        const matched = catalog.find((item) => item.id === cfg.catalogId)
        if (matched) {
          setSelectedItem(matched)
          setFormData(
            extractSchemaDefaults(
              matched.schema as Record<string, unknown>,
              cfg.values
            )
          )
          setChartVersion(cfg.version || matched.version || "")
        } else {
          setFormData(cfg.values || {})
          setChartVersion(cfg.version || "")
        }
        setTenant(cfg.tenant || "default")
        setInstanceName(cfg.name || initialReleaseTarget.name)
        if (cfg.targetCluster) {
          const matchesStack = stacks.some(
            (s) => `${s.name}-kubeconfig` === cfg.targetCluster
          )
          if (matchesStack) {
            setTargetCluster(cfg.targetCluster)
            setCustomClusterSecret("")
          } else {
            setTargetCluster("__custom__")
            setCustomClusterSecret(cfg.targetCluster)
          }
        } else {
          setTargetCluster("")
          setCustomClusterSecret("")
        }
        setTargetNamespace(cfg.targetNamespace || "")
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
  }, [initialReleaseTarget, catalog, stacks, onClearReleaseTarget])

  const handleFormChange = (data: { formData?: Record<string, unknown> }) => {
    if (data.formData) {
      setFormData(data.formData)
    }
  }

  const handleSyncUpstreamSchema = async () => {
    if (!selectedItem?.repoUrl) return
    const chartName = selectedItem.chart || selectedItem.id
    setSyncingSchema(true)
    setError(null)
    try {
      const { schema: upstreamSchema } = await fetchCatalogSchema({
        repoUrl: selectedItem.repoUrl,
        chart: chartName,
        version: selectedItem.version,
      })
      const existingSchema = (selectedItem.schema ?? {}) as Record<
        string,
        unknown
      >
      const existingProps =
        (existingSchema.properties as Record<string, unknown> | undefined) ?? {}
      const upstreamProps =
        (upstreamSchema.properties as Record<string, unknown> | undefined) ?? {}
      const mergedSchema: RJSFSchema = {
        ...upstreamSchema,
        required: Array.isArray(existingSchema.required)
          ? (existingSchema.required as string[])
          : (upstreamSchema.required as string[] | undefined),
        properties: {
          ...upstreamProps,
          ...existingProps,
        } as RJSFSchema["properties"],
      }
      const updatedItem: CatalogItem = { ...selectedItem, schema: mergedSchema }
      setSelectedItem(updatedItem)
      setCatalog((prev) =>
        prev.map((item) => (item.id === updatedItem.id ? updatedItem : item))
      )
      setFormData(
        extractSchemaDefaults(
          mergedSchema as Record<string, unknown>,
          updatedItem.defaultValues as Record<string, unknown> | undefined
        )
      )
    } catch (err: unknown) {
      setError(
        err instanceof Error
          ? err.message
          : "Failed to sync upstream Helm values schema"
      )
    } finally {
      setSyncingSchema(false)
    }
  }

  const handleSelectCatalogItem = (item: CatalogItem) => {
    setSelectedItem(item)
    setInstanceName(item.id)
    setChartVersion(item.version || "")
    setTargetNamespace("")
    setFormData(
      extractSchemaDefaults(
        item.schema as Record<string, unknown>,
        item.defaultValues as Record<string, unknown> | undefined
      )
    )
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
    setSchemaJsonText(
      JSON.stringify(item.schema ?? { type: "object", properties: {} }, null, 2)
    )
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
      runtime: (item.runtime as "helm" | "kustomize") || "helm",
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
    if (
      !appForm.id.trim() ||
      !appForm.name.trim() ||
      (!isInfra && !isBundle && !appForm.repoUrl?.trim()) ||
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
              repoUrl: appForm.repoUrl?.trim() || "",
              chart: isEditing
                ? (appForm.chart?.trim() ?? "")
                : appForm.chart?.trim() || undefined,
              version: isEditing
                ? (appForm.version?.trim() ?? "")
                : appForm.version?.trim() || undefined,
              ...(isEditing
                ? { path: appForm.path?.trim() ?? "" }
                : appForm.path?.trim()
                  ? { path: appForm.path.trim() }
                  : {}),
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
              repoUrl: payload.repoUrl,
              chart: payload.chart,
              version: payload.version,
              path: payload.path,
              flaggerSupported: payload.flaggerSupported,
              provider: payload.provider,
              pulumiRuntime: payload.pulumiRuntime,
              entrypoint: payload.entrypoint,
              protectByDefault: payload.protectByDefault,
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

  const handleSubmit = async (
    data?: { formData?: Record<string, unknown> } | React.FormEvent
  ) => {
    if (!selectedItem) return
    if (
      data &&
      "preventDefault" in data &&
      typeof data.preventDefault === "function"
    ) {
      data.preventDefault()
    }
    const valuesToSubmit =
      data && "formData" in data && data.formData ? data.formData : formData

    setLoading(true)
    setError(null)
    setCreatedProposal(null)

    try {
      const resolvedTargetCluster =
        selectedItem.type === "app"
          ? targetCluster === "__custom__"
            ? customClusterSecret.trim() || undefined
            : targetCluster || undefined
          : undefined

      const trimmedInstanceName = instanceName.trim()
      const trimmedChartVersion = chartVersion.trim()
      const trimmedTargetNs = resolvedTargetCluster
        ? targetNamespace.trim()
        : ""
      const isCustomName =
        Boolean(trimmedInstanceName) && trimmedInstanceName !== selectedItem.id

      const proposal = await createProposal({
        catalogId: selectedItem.id,
        tenant,
        values: valuesToSubmit,
        ...(isCustomName ? { name: trimmedInstanceName } : {}),
        ...(selectedItem.type === "app" &&
        trimmedChartVersion &&
        trimmedChartVersion !== selectedItem.version
          ? { version: trimmedChartVersion }
          : {}),
        ...(trimmedTargetNs ? { targetNamespace: trimmedTargetNs } : {}),
        title: `Deploy ${isCustomName ? trimmedInstanceName : selectedItem.name} for ${tenant}`,
        kind:
          selectedItem.type === "infrastructure"
            ? "infra"
            : (selectedItem.type as "app" | "infra" | "bundle"),
        ...(resolvedTargetCluster
          ? { targetCluster: resolvedTargetCluster }
          : {}),
      })
      setCreatedProposal(proposal)
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to create Git PR proposal"
      )
    } finally {
      setLoading(false)
    }
  }

  const yamlPreview = jsonToYaml(formData).trim()

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

          {tenant !== "default" && (
            <button
              type="button"
              onClick={() => handleDeleteTenant(tenant)}
              disabled={tenantLoading}
              className="px-2.5 py-1.5 bg-red-950/60 hover:bg-red-900/80 border border-red-500/30 text-red-300 rounded-lg text-xs font-medium transition-colors disabled:opacity-50 flex items-center gap-1"
            >
              <Trash2 className="h-3.5 w-3.5" />
              <span>Remove</span>
            </button>
          )}

          <button
            type="button"
            onClick={() => setIsNewTenantModalOpen(true)}
            className="px-3 py-1.5 bg-slate-800 hover:bg-slate-700 text-indigo-300 rounded-lg text-xs font-semibold transition-colors border border-slate-700 flex items-center gap-1"
          >
            <Plus className="h-3.5 w-3.5" />
            <span>+ New Tenant</span>
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

      {/* Main Catalog layout: Left catalog cards, Right RJSF form + YAML preview (independently scrollable panes) */}
      <div className="grid grid-cols-1 lg:grid-cols-12 gap-6 lg:h-[calc(100vh-14rem)] lg:min-h-[560px]">
        {/* Left Column: Search & Catalog List */}
        <div className="lg:col-span-4 flex flex-col lg:h-full min-h-0 space-y-4">
          <div className="flex items-center justify-between gap-2 shrink-0">
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
              className="px-3 py-2 bg-indigo-600 hover:bg-indigo-500 text-white rounded-xl text-xs font-semibold transition-colors shrink-0 flex items-center gap-1.5 shadow-md shadow-indigo-600/20"
            >
              <Plus className="h-4 w-4" />
              <span>+ Add Catalog Item</span>
            </button>
          </div>

          <div
            data-testid="catalog-list"
            className="space-y-3 flex-1 min-h-0 max-h-[640px] lg:max-h-none overflow-y-auto pr-1"
          >
            {filteredCatalog.map((item) => {
              const isSelected =
                selectedItem?.id === item.id && selectedItem?.type === item.type
              return (
                <div
                  key={`${item.type}-${item.id}`}
                  onClick={() => handleSelectCatalogItem(item)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      handleSelectCatalogItem(item)
                    }
                  }}
                  className={`p-4 rounded-xl border text-left cursor-pointer transition-all ${
                    isSelected
                      ? "bg-indigo-950/30 border-indigo-500/50 shadow-lg shadow-indigo-950/40"
                      : "bg-slate-900 border-slate-800 hover:border-slate-700 hover:bg-slate-800/40"
                  }`}
                >
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-xs font-bold text-white flex items-center gap-1.5">
                      <Box className="h-3.5 w-3.5 text-indigo-400" />
                      {item.name}
                    </span>
                    <span className="text-[10px] font-mono px-2 py-0.5 rounded bg-slate-800 text-slate-400">
                      {item.type === "bundle" ? "bundle" : `v${item.version}`}
                    </span>
                  </div>
                  <p className="text-[11px] text-slate-400 line-clamp-2 mb-2">
                    {item.description}
                  </p>
                  <div className="flex items-center justify-between text-[10px]">
                    <span className="px-2 py-0.5 rounded-full bg-slate-800 text-slate-300 font-medium">
                      {item.category}
                    </span>
                    {item.provider && (
                      <span className="uppercase font-mono text-indigo-400">
                        {item.provider}
                      </span>
                    )}
                  </div>
                </div>
              )
            })}
            {filteredCatalog.length === 0 && (
              <div className="p-4 rounded-xl bg-slate-900 border border-slate-800 text-xs text-slate-500 text-center">
                {catalog.length === 0
                  ? "No catalog blueprints available — check server configuration or authenticate."
                  : `No catalog blueprints matching "${searchQuery}".`}
              </div>
            )}
          </div>
        </div>

        {/* Right Column: Dynamic JSONSchema Form + Live values.yaml Preview */}
        <div className="lg:col-span-8 flex flex-col lg:h-full min-h-0">
          {selectedItem ? (
            <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 flex flex-col lg:h-full min-h-0">
              <div className="flex items-center justify-between border-b border-slate-800 pb-4 shrink-0">
                <div>
                  <h3 className="text-lg font-semibold text-white flex items-center gap-2">
                    <span>{selectedItem.name}</span>
                    <span className="text-xs font-normal text-slate-400">
                      ({selectedItem.type})
                    </span>
                  </h3>
                  <p className="text-xs text-slate-400 mt-0.5">
                    {selectedItem.description}
                  </p>
                </div>
                <div className="flex items-center space-x-2">
                  <button
                    type="button"
                    onClick={() => openEditCatalogModal(selectedItem)}
                    className="flex items-center space-x-1.5 px-2.5 py-1 rounded-lg text-xs font-medium bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer"
                    title="Edit catalog item definition via Git PR"
                  >
                    <Pencil className="h-3 w-3" />
                    <span>Edit</span>
                  </button>
                  {selectedItem.type === "app" && selectedItem.repoUrl && (
                    <button
                      type="button"
                      onClick={handleSyncUpstreamSchema}
                      disabled={syncingSchema}
                      className="flex items-center space-x-1.5 px-2.5 py-1 rounded-lg text-xs font-medium bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer disabled:opacity-50"
                      title="Pull values.schema.json or values.yaml from upstream Helm chart"
                    >
                      <RefreshCw
                        className={`h-3 w-3 ${syncingSchema ? "animate-spin" : ""}`}
                      />
                      <span>
                        {syncingSchema ? "Syncing..." : "Sync Upstream Schema"}
                      </span>
                    </button>
                  )}
                  <span className="text-xs font-mono px-3 py-1 rounded-lg bg-indigo-500/10 text-indigo-400 border border-indigo-500/20">
                    {selectedItem.id}
                  </span>
                </div>
              </div>

              <div className="flex-1 min-h-0 overflow-y-auto pr-2 space-y-5 mt-5">
                {selectedItem.type === "bundle" &&
                  selectedItem.bundleItems &&
                  selectedItem.bundleItems.length > 0 && (
                    <div className="bg-indigo-950/20 border border-indigo-500/30 rounded-xl p-3.5 space-y-2">
                      <div className="text-[11px] font-semibold uppercase tracking-wider text-indigo-300">
                        Bundle Execution Graph (
                        {selectedItem.bundleItems.length} items)
                      </div>
                      <div className="flex flex-wrap gap-2">
                        {selectedItem.bundleItems.map((bi) => (
                          <div
                            key={bi.id}
                            className="text-[11px] font-mono bg-slate-950 border border-slate-800 rounded-lg px-2.5 py-1.5 text-slate-200 flex items-center gap-1.5"
                          >
                            <span className="px-1.5 py-0.5 rounded bg-indigo-500/20 text-indigo-300 text-[10px] uppercase">
                              {bi.kind}
                            </span>
                            <span className="font-semibold text-white">
                              {bi.id}
                            </span>
                            <span className="text-slate-500">
                              ({bi.catalogRef})
                            </span>
                            {bi.dependsOn && bi.dependsOn.length > 0 && (
                              <span className="text-amber-300 text-[10px]">
                                ← {bi.dependsOn.join(", ")}
                              </span>
                            )}
                          </div>
                        ))}
                      </div>
                    </div>
                  )}

                {/* Deployment Instance Metadata */}
                <div
                  className={`grid grid-cols-1 gap-3 ${
                    selectedItem.type === "app"
                      ? "sm:grid-cols-2"
                      : "sm:grid-cols-1"
                  }`}
                >
                  <div>
                    <label
                      htmlFor="instance-name-input"
                      className="block text-[11px] font-medium text-slate-300 mb-1"
                    >
                      Instance Name
                    </label>
                    <input
                      id="instance-name-input"
                      type="text"
                      value={instanceName}
                      onChange={(e) => setInstanceName(e.target.value)}
                      placeholder={selectedItem.id}
                      className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                    />
                  </div>
                  {selectedItem.type === "app" && (
                    <>
                      <div>
                        <label
                          htmlFor="chart-version-input"
                          className="block text-[11px] font-medium text-slate-300 mb-1"
                        >
                          Chart Version
                        </label>
                        <input
                          id="chart-version-input"
                          type="text"
                          value={chartVersion}
                          onChange={(e) => setChartVersion(e.target.value)}
                          placeholder={selectedItem.version || "latest"}
                          className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                        />
                      </div>
                      <div>
                        <label
                          htmlFor="cluster-select-catalog"
                          className="block text-[11px] font-medium text-slate-300 mb-1"
                        >
                          Target Cluster
                        </label>
                        <select
                          id="cluster-select-catalog"
                          value={targetCluster}
                          onChange={(e) => setTargetCluster(e.target.value)}
                          className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                        >
                          <option value="">Management Cluster (local)</option>
                          {stacks.map((s) => (
                            <option key={s.id} value={`${s.name}-kubeconfig`}>
                              {s.name} ({s.tenant})
                            </option>
                          ))}
                          <option value="__custom__">
                            Custom kubeconfig Secret...
                          </option>
                        </select>
                        {targetCluster === "__custom__" && (
                          <input
                            type="text"
                            value={customClusterSecret}
                            onChange={(e) =>
                              setCustomClusterSecret(e.target.value)
                            }
                            placeholder="e.g. workload-kubeconfig"
                            aria-label="Custom kubeconfig Secret name"
                            className="mt-2 w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                          />
                        )}
                      </div>
                      <div>
                        <label
                          htmlFor="target-namespace-input"
                          className="block text-[11px] font-medium text-slate-300 mb-1"
                        >
                          Target Namespace
                        </label>
                        <input
                          id="target-namespace-input"
                          type="text"
                          value={targetCluster ? targetNamespace : ""}
                          onChange={(e) => setTargetNamespace(e.target.value)}
                          disabled={!targetCluster}
                          placeholder={
                            targetCluster
                              ? "e.g. apps-prod (optional)"
                              : `tenant-${tenant} (local)`
                          }
                          title={
                            targetCluster
                              ? "Destination namespace on the remote workload cluster"
                              : "Management cluster apps are isolated to the tenant namespace; select a remote Target Cluster to override"
                          }
                          className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono disabled:opacity-50 disabled:cursor-not-allowed"
                        />
                      </div>
                    </>
                  )}
                </div>

                {(() => {
                  const effectiveInstanceName =
                    instanceName.trim() || selectedItem.id
                  const isExistingInstance =
                    selectedItem.type === "app"
                      ? releases.some(
                          (r) =>
                            r.name === effectiveInstanceName &&
                            r.namespace === `tenant-${tenant}`
                        )
                      : stacks.some(
                          (s) =>
                            s.name === effectiveInstanceName &&
                            s.tenant === tenant
                        )
                  if (!isExistingInstance) return null
                  return (
                    <div
                      role="status"
                      className="flex items-start space-x-2 p-3 rounded-xl bg-amber-950/30 border border-amber-500/30 text-amber-200 text-xs"
                    >
                      <AlertTriangle className="h-4 w-4 text-amber-400 shrink-0 mt-0.5" />
                      <span>
                        Instance{" "}
                        <code className="font-mono font-semibold">
                          {effectiveInstanceName}
                        </code>{" "}
                        already exists in{" "}
                        <code className="font-mono">tenant-{tenant}</code> —
                        submitting will propose an update to it. Change{" "}
                        <strong>Instance Name</strong> to deploy a separate
                        copy.
                      </span>
                    </div>
                  )
                })()}

                {/* RJSF Form Container */}
                <div className="rjsf-container bg-slate-950 p-5 rounded-xl border border-slate-800">
                  <Form
                    key={selectedItem.id}
                    schema={selectedItem.schema as RJSFSchema}
                    templates={{ ButtonTemplates: RJSF_BUTTON_TEMPLATES }}
                    validator={
                      validator as unknown as ValidatorType<
                        Record<string, unknown>,
                        RJSFSchema
                      >
                    }
                    formData={formData}
                    onChange={handleFormChange}
                    onSubmit={handleSubmit}
                  >
                    <div className="mt-4 pt-4 border-t border-slate-800 flex items-center justify-end">
                      <button
                        type="submit"
                        disabled={loading}
                        className="flex items-center space-x-2 px-6 py-2.5 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-semibold rounded-lg transition-colors shadow-lg shadow-indigo-600/20 disabled:opacity-50"
                      >
                        {loading ? (
                          <>
                            <Loader2 className="h-4 w-4 animate-spin" />
                            <span>Opening PR Proposal...</span>
                          </>
                        ) : (
                          <>
                            <span>Submit &amp; Open Git PR Proposal</span>
                            <ArrowRight className="h-4 w-4" />
                          </>
                        )}
                      </button>
                    </div>
                  </Form>
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

                {/* Live values.yaml Diff Preview */}
                <div className="space-y-2">
                  <div className="flex items-center justify-between">
                    <h4 className="text-xs font-semibold text-slate-300 flex items-center gap-1.5 uppercase tracking-wider">
                      <FileCode2 className="h-4 w-4 text-emerald-400" />
                      Live values.yaml Preview
                    </h4>
                    <span className="text-[10px] text-slate-500 font-mono">
                      SOPS + age encrypted on secret fields before git commit
                    </span>
                  </div>
                  <pre
                    aria-label="values.yaml preview"
                    className="bg-slate-950 border border-slate-800 rounded-xl p-4 text-xs font-mono text-emerald-400 overflow-x-auto max-h-56 leading-relaxed"
                  >
                    {yamlPreview}
                  </pre>
                </div>
              </div>
            </div>
          ) : (
            <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 text-center text-slate-400 text-xs space-y-4">
              {error && (
                <div
                  role="alert"
                  className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-start space-x-2 text-left"
                >
                  <AlertCircle className="h-4 w-4 text-red-400 shrink-0 mt-0.5" />
                  <span>{error}</span>
                </div>
              )}
              <p>
                No catalog blueprint selected. Select an item from the catalog
                or authenticate to view available blueprints.
              </p>
            </div>
          )}
        </div>
      </div>

      {/* New Tenant Modal */}
      {isNewTenantModalOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm p-4">
          <div className="bg-slate-900 border border-slate-800 rounded-2xl max-w-md w-full p-6 space-y-4 shadow-2xl">
            <div className="flex items-center justify-between border-b border-slate-800 pb-3">
              <h3 className="text-base font-bold text-white flex items-center gap-2">
                <Plus className="h-4 w-4 text-indigo-400" />
                Create New Tenant
              </h3>
              <button
                type="button"
                onClick={() => {
                  setIsNewTenantModalOpen(false)
                  setTenantError(null)
                }}
                className="text-slate-400 hover:text-white transition-colors"
              >
                <X className="h-5 w-5" />
              </button>
            </div>

            <form onSubmit={handleCreateTenant} className="space-y-4">
              <div>
                <label
                  htmlFor="new-tenant-id"
                  className="block text-xs text-slate-300 font-medium mb-1"
                >
                  Tenant ID
                </label>
                <input
                  id="new-tenant-id"
                  type="text"
                  required
                  placeholder="e.g. acme"
                  value={newTenantId}
                  onChange={(e) => setNewTenantId(e.target.value)}
                  className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                />
              </div>

              {tenantError && (
                <div className="p-3 bg-red-950/40 border border-red-500/30 rounded-lg text-red-300 text-xs flex items-center gap-2">
                  <AlertCircle className="h-4 w-4 text-red-400 shrink-0" />
                  <span>{tenantError}</span>
                </div>
              )}

              <div className="flex items-center justify-end gap-3 pt-2">
                <button
                  type="button"
                  onClick={() => {
                    setIsNewTenantModalOpen(false)
                    setTenantError(null)
                  }}
                  className="px-4 py-2 bg-slate-800 hover:bg-slate-700 text-slate-300 text-xs font-medium rounded-lg transition-colors"
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  disabled={tenantLoading || !newTenantId.trim()}
                  className="px-4 py-2 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-semibold rounded-lg transition-colors disabled:opacity-50 flex items-center gap-1.5"
                >
                  {tenantLoading ? (
                    <>
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                      <span>Creating...</span>
                    </>
                  ) : (
                    <span>Create Tenant PR</span>
                  )}
                </button>
              </div>
            </form>
          </div>
        </div>
      )}

      {/* Add Catalog Item Modal */}
      {isAddAppModalOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm p-4 overflow-y-auto">
          <div className="bg-slate-900 border border-slate-800 rounded-2xl max-w-lg w-full p-6 space-y-4 shadow-2xl my-8">
            <div className="flex items-center justify-between border-b border-slate-800 pb-3">
              <h3 className="text-base font-bold text-white flex items-center gap-2">
                <Box className="h-4 w-4 text-indigo-400" />
                {editingCatalogItem ? "Edit Catalog Item" : "Add Catalog Item"}
              </h3>
              <button
                type="button"
                onClick={() => {
                  setIsAddAppModalOpen(false)
                  setEditingCatalogItem(null)
                  setAppError(null)
                }}
                aria-label="Close modal"
                className="text-slate-400 hover:text-white transition-colors"
              >
                <X className="h-5 w-5" />
              </button>
            </div>

            <form onSubmit={handleCreateCatalogApp} className="space-y-4">
              <div>
                <label
                  htmlFor="catalog-type"
                  className="block text-xs text-slate-300 font-medium mb-1"
                >
                  Catalog Type *
                </label>
                <select
                  id="catalog-type"
                  disabled={Boolean(editingCatalogItem)}
                  value={appForm.kind || "app"}
                  onChange={(e) => {
                    const newKind = e.target.value as "app" | "infra" | "bundle"
                    setAppForm((prev) => ({
                      ...prev,
                      kind: newKind,
                      category:
                        newKind === "infra"
                          ? "Infrastructure"
                          : newKind === "bundle"
                            ? "Platform"
                            : "Applications",
                      items:
                        newKind === "bundle" &&
                        (!prev.items || prev.items.length === 0)
                          ? [
                              {
                                id: "step-1",
                                kind: catalog.some(
                                  (c) =>
                                    c.type === "infra" ||
                                    c.type === "infrastructure"
                                )
                                  ? "infra"
                                  : "app",
                                catalogRef:
                                  catalog.find(
                                    (c) =>
                                      c.type === "infra" ||
                                      c.type === "infrastructure"
                                  )?.id ??
                                  catalog.find((c) => c.type === "app")?.id ??
                                  "",
                                dependsOn: [],
                              },
                            ]
                          : prev.items,
                    }))
                  }}
                  className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                >
                  <option value="app">Application (Helm / Kustomize)</option>
                  <option value="infra">
                    Infrastructure Blueprint (PKO v2)
                  </option>
                  <option value="bundle">
                    Multi-Item Bundle (Infra + Apps)
                  </option>
                </select>
              </div>

              <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                <div>
                  <label
                    htmlFor="app-id"
                    className="block text-xs text-slate-300 font-medium mb-1"
                  >
                    Item ID *
                  </label>
                  <input
                    id="app-id"
                    type="text"
                    required
                    disabled={Boolean(editingCatalogItem)}
                    placeholder="e.g. podinfo"
                    value={appForm.id}
                    onChange={(e) =>
                      setAppForm((prev) => ({ ...prev, id: e.target.value }))
                    }
                    className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono disabled:opacity-60"
                  />
                </div>

                <div>
                  <label
                    htmlFor="app-name"
                    className="block text-xs text-slate-300 font-medium mb-1"
                  >
                    Item Name *
                  </label>
                  <input
                    id="app-name"
                    type="text"
                    required
                    placeholder={
                      appForm.kind === "infra"
                        ? "e.g. Proxmox Cluster Blueprint"
                        : appForm.kind === "bundle"
                          ? "e.g. kind Cluster + Podinfo Stack"
                          : "e.g. Podinfo Microservice"
                    }
                    value={appForm.name}
                    onChange={(e) =>
                      setAppForm((prev) => ({ ...prev, name: e.target.value }))
                    }
                    className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                  />
                </div>
              </div>

              {(appForm.kind || "app") === "app" && (
                <>
                  <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                    <div>
                      <label
                        htmlFor="app-category"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Category *
                      </label>
                      <input
                        id="app-category"
                        type="text"
                        required
                        placeholder="Applications"
                        value={appForm.category}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            category: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      />
                    </div>

                    <div>
                      <label
                        htmlFor="app-runtime"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Runtime *
                      </label>
                      <select
                        id="app-runtime"
                        value={appForm.runtime || "helm"}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            runtime: e.target.value as "helm" | "kustomize",
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      >
                        <option value="helm">helm</option>
                        <option value="kustomize">kustomize</option>
                      </select>
                    </div>
                  </div>

                  <div>
                    <label
                      htmlFor="app-repourl"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Repository URL *
                    </label>
                    <input
                      id="app-repourl"
                      type="text"
                      required
                      placeholder="https://github.com/stefanprodan/podinfo"
                      value={appForm.repoUrl || ""}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          repoUrl: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                    />
                  </div>

                  <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                    <div>
                      <label
                        htmlFor="app-chart"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Chart Name (Helm) / Path
                      </label>
                      <input
                        id="app-chart"
                        type="text"
                        placeholder="podinfo"
                        value={appForm.chart || ""}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            chart: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                      />
                    </div>

                    <div>
                      <label
                        htmlFor="app-version"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Version
                      </label>
                      <input
                        id="app-version"
                        type="text"
                        placeholder="6.7.0"
                        value={appForm.version || ""}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            version: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                      />
                    </div>
                  </div>

                  <div className="flex items-center space-x-2 pt-1">
                    <input
                      id="app-flagger"
                      type="checkbox"
                      checked={!!appForm.flaggerSupported}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          flaggerSupported: e.target.checked,
                        }))
                      }
                      className="h-4 w-4 rounded border-slate-800 bg-slate-950 text-indigo-600 focus:ring-indigo-500"
                    />
                    <label
                      htmlFor="app-flagger"
                      className="text-xs text-slate-300 font-medium cursor-pointer"
                    >
                      Flagger Canary Support Enabled
                    </label>
                  </div>

                  <div>
                    <label
                      htmlFor="app-description"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Description
                    </label>
                    <input
                      id="app-description"
                      type="text"
                      placeholder="Optional summary..."
                      value={appForm.description || ""}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          description: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>
                </>
              )}

              {appForm.kind === "infra" && (
                <>
                  <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                    <div>
                      <label
                        htmlFor="app-category"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Category *
                      </label>
                      <input
                        id="app-category"
                        type="text"
                        required
                        placeholder="Infrastructure"
                        value={appForm.category}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            category: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      />
                    </div>

                    <div>
                      <label
                        htmlFor="infra-provider"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Provider *
                      </label>
                      <select
                        id="infra-provider"
                        value={appForm.provider || "kind"}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            provider: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      >
                        <option value="kind">kind</option>
                        <option value="aws">aws</option>
                        <option value="proxmox">proxmox</option>
                        <option value="gcp">gcp</option>
                        <option value="azure">azure</option>
                        <option value="hetzner">hetzner</option>
                        <option value="cloudflare">cloudflare</option>
                        <option value="custom">custom</option>
                      </select>
                    </div>
                  </div>

                  <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                    <div>
                      <label
                        htmlFor="infra-pulumi-runtime"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Pulumi Runtime *
                      </label>
                      <select
                        id="infra-pulumi-runtime"
                        value={appForm.pulumiRuntime || "yaml"}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            pulumiRuntime: e.target.value as
                              "yaml" | "hcl" | "opentofu-bridge",
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      >
                        <option value="yaml">yaml</option>
                        <option value="hcl">hcl</option>
                        <option value="opentofu-bridge">opentofu-bridge</option>
                      </select>
                    </div>

                    <div>
                      <label
                        htmlFor="infra-entrypoint"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Entrypoint
                      </label>
                      <input
                        id="infra-entrypoint"
                        type="text"
                        placeholder="Pulumi.yaml"
                        value={appForm.entrypoint ?? "Pulumi.yaml"}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            entrypoint: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                      />
                    </div>
                  </div>

                  <div>
                    <label
                      htmlFor="infra-tf-provider"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      OpenTofu / Terraform Provider
                    </label>
                    <input
                      id="infra-tf-provider"
                      type="text"
                      placeholder="e.g. tehcyx/kind, hashicorp/aws, bpg/proxmox"
                      value={appForm.terraformProvider || ""}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          terraformProvider: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                    />
                  </div>

                  <div className="flex items-center space-x-2 pt-1">
                    <input
                      id="infra-protect"
                      type="checkbox"
                      checked={!!appForm.protectByDefault}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          protectByDefault: e.target.checked,
                        }))
                      }
                      className="h-4 w-4 rounded border-slate-800 bg-slate-950 text-indigo-600 focus:ring-indigo-500"
                    />
                    <label
                      htmlFor="infra-protect"
                      className="text-xs text-slate-300 font-medium cursor-pointer"
                    >
                      Protect by Default
                    </label>
                  </div>

                  <div>
                    <label
                      htmlFor="infra-description"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Description
                    </label>
                    <input
                      id="infra-description"
                      type="text"
                      placeholder="Optional summary..."
                      value={appForm.description || ""}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          description: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>
                </>
              )}

              {appForm.kind === "bundle" && (
                <div className="space-y-3">
                  <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                    <div>
                      <label
                        htmlFor="bundle-category"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Category *
                      </label>
                      <input
                        id="bundle-category"
                        type="text"
                        required
                        value={appForm.category}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            category: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      />
                    </div>
                    <div>
                      <label
                        htmlFor="bundle-description"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Description
                      </label>
                      <input
                        id="bundle-description"
                        type="text"
                        placeholder="Optional summary..."
                        value={appForm.description || ""}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            description: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      />
                    </div>
                  </div>

                  <div className="space-y-2">
                    <div className="flex items-center justify-between">
                      <span className="text-xs font-medium text-slate-300">
                        Bundle Items &amp; Dependency Chain
                      </span>
                      <button
                        type="button"
                        onClick={() =>
                          setAppForm((prev) => {
                            const existing = prev.items ?? []
                            const nextIdx = existing.length + 1
                            return {
                              ...prev,
                              items: [
                                ...existing,
                                {
                                  id: `step-${nextIdx}`,
                                  kind: "app",
                                  catalogRef:
                                    catalog.find((c) => c.type === "app")?.id ??
                                    "",
                                  dependsOn: existing[0]
                                    ? [existing[0].id]
                                    : [],
                                },
                              ],
                            }
                          })
                        }
                        className="px-2.5 py-1 bg-slate-800 hover:bg-slate-700 text-indigo-300 rounded-lg text-[11px] font-medium flex items-center gap-1"
                      >
                        <Plus className="h-3 w-3" />
                        <span>Add Step</span>
                      </button>
                    </div>

                    <div className="space-y-2.5 max-h-60 overflow-y-auto pr-1">
                      {(appForm.items ?? []).map((item, idx) => {
                        const availableRefs = catalog.filter((c) =>
                          item.kind === "infra"
                            ? c.type === "infra" || c.type === "infrastructure"
                            : c.type === "app"
                        )
                        const priorItems = (appForm.items ?? []).filter(
                          (_, i) => i !== idx
                        )
                        const infraDeps = priorItems.filter(
                          (p) =>
                            p.kind === "infra" &&
                            (item.dependsOn ?? []).includes(p.id)
                        )
                        const updateItem = (
                          patch: Partial<CatalogBundleItem>
                        ) => {
                          setAppForm((prev) => ({
                            ...prev,
                            items: (prev.items ?? []).map((it, i) =>
                              i === idx ? { ...it, ...patch } : it
                            ),
                          }))
                        }
                        return (
                          <div
                            key={idx}
                            className="p-3 bg-slate-950 border border-slate-800 rounded-xl space-y-2"
                          >
                            <div className="grid grid-cols-12 gap-2 items-center">
                              <input
                                type="text"
                                aria-label={`Step ${idx + 1} ID`}
                                placeholder="step id"
                                value={item.id}
                                onChange={(e) =>
                                  updateItem({ id: e.target.value })
                                }
                                className="col-span-3 px-2 py-1.5 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-100"
                              />
                              <select
                                aria-label={`Step ${idx + 1} Kind`}
                                value={item.kind}
                                onChange={(e) => {
                                  const nextKind = e.target.value as
                                    "infra" | "app"
                                  const firstRef = catalog.find((c) =>
                                    nextKind === "infra"
                                      ? c.type === "infra" ||
                                        c.type === "infrastructure"
                                      : c.type === "app"
                                  )?.id
                                  updateItem({
                                    kind: nextKind,
                                    catalogRef: firstRef ?? "",
                                    targetClusterFrom: undefined,
                                    targetNamespace: undefined,
                                  })
                                }}
                                className="col-span-3 px-2 py-1.5 bg-slate-900 border border-slate-800 rounded-lg text-[11px] text-slate-100"
                              >
                                <option value="infra">infra</option>
                                <option value="app">app</option>
                              </select>
                              <select
                                aria-label={`Step ${idx + 1} Catalog Ref`}
                                value={item.catalogRef}
                                onChange={(e) =>
                                  updateItem({ catalogRef: e.target.value })
                                }
                                className="col-span-5 px-2 py-1.5 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-100"
                              >
                                <option value="">Select item...</option>
                                {availableRefs.map((c) => (
                                  <option key={c.id} value={c.id}>
                                    {c.id} ({c.name})
                                  </option>
                                ))}
                              </select>
                              <button
                                type="button"
                                aria-label={`Remove step ${idx + 1}`}
                                onClick={() =>
                                  setAppForm((prev) => ({
                                    ...prev,
                                    items: (prev.items ?? []).filter(
                                      (_, i) => i !== idx
                                    ),
                                  }))
                                }
                                className="col-span-1 p-1 text-slate-400 hover:text-red-400 flex justify-center"
                              >
                                <Trash2 className="h-3.5 w-3.5" />
                              </button>
                            </div>

                            {priorItems.length > 0 && (
                              <div className="flex flex-wrap items-center gap-2 text-[11px] text-slate-400">
                                <span>Depends on:</span>
                                {priorItems.map((p) => {
                                  const checked = (
                                    item.dependsOn ?? []
                                  ).includes(p.id)
                                  return (
                                    <label
                                      key={p.id}
                                      className="inline-flex items-center gap-1 cursor-pointer font-mono text-slate-300"
                                    >
                                      <input
                                        type="checkbox"
                                        checked={checked}
                                        onChange={(e) => {
                                          const current = item.dependsOn ?? []
                                          const next = e.target.checked
                                            ? [...current, p.id]
                                            : current.filter((d) => d !== p.id)
                                          updateItem({
                                            dependsOn: next,
                                            targetClusterFrom:
                                              item.targetClusterFrom &&
                                              !next.includes(
                                                item.targetClusterFrom
                                              )
                                                ? undefined
                                                : item.targetClusterFrom,
                                          })
                                        }}
                                        className="rounded border-slate-700 bg-slate-900 text-indigo-500"
                                      />
                                      <span>{p.id}</span>
                                    </label>
                                  )
                                })}
                              </div>
                            )}

                            {item.kind === "app" && (
                              <div className="grid grid-cols-2 gap-2">
                                <select
                                  aria-label={`Step ${idx + 1} Target Cluster From`}
                                  value={item.targetClusterFrom || ""}
                                  onChange={(e) =>
                                    updateItem({
                                      targetClusterFrom:
                                        e.target.value || undefined,
                                    })
                                  }
                                  className="px-2 py-1 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-200"
                                >
                                  <option value="">
                                    Local / Tenant Cluster
                                  </option>
                                  {infraDeps.map((inf) => (
                                    <option key={inf.id} value={inf.id}>
                                      From infra: {inf.id}
                                    </option>
                                  ))}
                                </select>
                                <input
                                  type="text"
                                  aria-label={`Step ${idx + 1} Target Namespace`}
                                  placeholder="Target namespace (optional)"
                                  value={item.targetNamespace || ""}
                                  onChange={(e) =>
                                    updateItem({
                                      targetNamespace:
                                        e.target.value || undefined,
                                    })
                                  }
                                  className="px-2 py-1 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-200"
                                />
                              </div>
                            )}
                          </div>
                        )
                      })}
                    </div>
                  </div>
                </div>
              )}

              <div className="pt-1 border-t border-slate-800/80">
                <button
                  type="button"
                  onClick={() => {
                    if (!showSchemaEditor && !schemaJsonText) {
                      setSchemaJsonText(
                        JSON.stringify(
                          { type: "object", properties: {} },
                          null,
                          2
                        )
                      )
                    }
                    setShowSchemaEditor((prev) => !prev)
                  }}
                  className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 transition-colors cursor-pointer"
                >
                  {showSchemaEditor
                    ? "Hide JSON Schema Editor"
                    : "Edit JSON Schema (Optional)"}
                </button>
                {showSchemaEditor && (
                  <div className="mt-2">
                    <label
                      htmlFor="catalog-schema-json"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      JSON Schema
                    </label>
                    <textarea
                      id="catalog-schema-json"
                      rows={6}
                      value={schemaJsonText}
                      onChange={(e) => setSchemaJsonText(e.target.value)}
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-[11px] text-slate-100 font-mono focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>
                )}
              </div>

              {appError && (
                <div className="p-3 bg-red-950/40 border border-red-500/30 rounded-lg text-red-300 text-xs flex items-center gap-2">
                  <AlertCircle className="h-4 w-4 text-red-400 shrink-0" />
                  <span>{appError}</span>
                </div>
              )}

              <div className="flex items-center justify-end gap-3 pt-2">
                <button
                  type="button"
                  onClick={() => {
                    setIsAddAppModalOpen(false)
                    setEditingCatalogItem(null)
                    setAppError(null)
                  }}
                  className="px-4 py-2 bg-slate-800 hover:bg-slate-700 text-slate-300 text-xs font-medium rounded-lg transition-colors"
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  disabled={
                    appLoading ||
                    !appForm.id.trim() ||
                    !appForm.name.trim() ||
                    (appForm.kind !== "infra" &&
                      appForm.kind !== "bundle" &&
                      !appForm.repoUrl?.trim()) ||
                    (appForm.kind === "bundle" &&
                      (!appForm.items ||
                        appForm.items.length === 0 ||
                        appForm.items.some(
                          (it) => !it.id.trim() || !it.catalogRef.trim()
                        )))
                  }
                  className="px-4 py-2 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-semibold rounded-lg transition-colors disabled:opacity-50 flex items-center gap-1.5"
                >
                  {appLoading ? (
                    <>
                      <Loader2 className="h-3.5 w-3.5 animate-spin" />
                      <span>Opening PR...</span>
                    </>
                  ) : (
                    <span>
                      {editingCatalogItem
                        ? "Submit Update PR"
                        : appForm.kind === "infra"
                          ? "Submit Infra PR Proposal"
                          : appForm.kind === "bundle"
                            ? "Submit Bundle PR Proposal"
                            : "Submit App PR Proposal"}
                    </span>
                  )}
                </button>
              </div>
            </form>
          </div>
        </div>
      )}
    </div>
  )
}
