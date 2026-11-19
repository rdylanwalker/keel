import React, { useEffect, useState } from "react"
import type { RJSFSchema } from "@rjsf/utils"
import {
  Search,
  Layers,
  CheckCircle2,
  AlertCircle,
  Loader2,
  ArrowRight,
  GitPullRequest,
  Box,
  Plus,
  Trash2,
  X,
  Pencil,
  Maximize2,
  Minimize2,
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
  PROVIDER_CREDENTIAL_PRESETS,
  SchemaFieldDraft,
  deriveFieldsFromPulumiYaml,
  extractManifestVariables,
  fieldsToSchemaJson,
  humanizeFieldName,
  schemaJsonToFields,
} from "../lib/schemaBuilder"
import { SchemaFieldsEditor } from "./SchemaFieldsEditor"
import {
  DeployInstanceInitialConfig,
  DeployInstanceModal,
} from "./DeployInstanceModal"

const DEFAULT_STARTER_MANIFEST = `apiVersion: apps/v1
kind: Deployment
metadata:
  name: \${appName}
  labels:
    app: \${appName}
spec:
  replicas: \${replicas:=2}
  selector:
    matchLabels:
      app: \${appName}
  template:
    metadata:
      labels:
        app: \${appName}
    spec:
      containers:
        - name: app
          image: nginx:alpine
          ports:
            - containerPort: 80
---
apiVersion: v1
kind: Service
metadata:
  name: \${appName}
spec:
  selector:
    app: \${appName}
  ports:
    - port: 80
      targetPort: 80`

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
  const [isTenantModalFullscreen, setIsTenantModalFullscreen] = useState(false)
  const [newTenantId, setNewTenantId] = useState("")
  const [tenantLoading, setTenantLoading] = useState(false)
  const [tenantError, setTenantError] = useState<string | null>(null)

  const [isAddAppModalOpen, setIsAddAppModalOpen] = useState(false)
  const [isAddModalFullscreen, setIsAddModalFullscreen] = useState(false)
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
                  }
                : {}),
              repoUrl:
                appForm.runtime === "manifest" && !appForm.repoUrl?.trim()
                  ? "https://kubernetes.io/docs/concepts/overview/working-with-objects/"
                  : appForm.repoUrl?.trim() || "",
              chart: isEditing
                ? (appForm.chart?.trim() ?? "")
                : appForm.chart?.trim() || undefined,
              version: isEditing
                ? (appForm.version?.trim() ?? "")
                : appForm.version?.trim() || undefined,
              ...(isEditing
                ? {
                    path:
                      appForm.runtime === "manifest" && !appForm.path?.trim()
                        ? "./manifests.yaml"
                        : (appForm.path?.trim() ?? ""),
                  }
                : {
                    path:
                      appForm.runtime === "manifest" && !appForm.path?.trim()
                        ? "./manifests.yaml"
                        : appForm.path?.trim() || undefined,
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
      {isNewTenantModalOpen && (
        <div
          className={`fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm ${
            isTenantModalFullscreen ? "p-0" : "p-4"
          }`}
        >
          <div
            data-testid="new-tenant-modal"
            className={`bg-slate-900 border-slate-800 flex flex-col space-y-4 shadow-2xl ${
              isTenantModalFullscreen
                ? "w-full h-full rounded-none border-0 p-6"
                : "border rounded-2xl max-w-4xl w-full p-6"
            }`}
          >
            <div className="flex items-center justify-between border-b border-slate-800 pb-3 shrink-0">
              <h3 className="text-base font-bold text-white flex items-center gap-2">
                <Plus className="h-4 w-4 text-indigo-400" />
                Create New Tenant
              </h3>
              <div className="flex items-center gap-1.5">
                <button
                  type="button"
                  onClick={() => setIsTenantModalFullscreen((prev) => !prev)}
                  aria-label={
                    isTenantModalFullscreen
                      ? "Exit full screen"
                      : "Enter full screen"
                  }
                  title={
                    isTenantModalFullscreen ? "Exit full screen" : "Full screen"
                  }
                  className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
                >
                  {isTenantModalFullscreen ? (
                    <Minimize2 className="h-4 w-4" />
                  ) : (
                    <Maximize2 className="h-4 w-4" />
                  )}
                </button>
                <button
                  type="button"
                  onClick={() => {
                    setIsNewTenantModalOpen(false)
                    setIsTenantModalFullscreen(false)
                    setTenantError(null)
                  }}
                  aria-label="Close modal"
                  className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
                >
                  <X className="h-5 w-5" />
                </button>
              </div>
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
        <div
          className={`fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm ${
            isAddModalFullscreen ? "p-0" : "p-4 overflow-y-auto"
          }`}
        >
          <div
            data-testid="catalog-item-modal"
            className={`bg-slate-900 border-slate-800 flex flex-col space-y-4 shadow-2xl ${
              isAddModalFullscreen
                ? "w-full h-full rounded-none border-0 p-6 overflow-y-auto"
                : "border rounded-2xl max-w-4xl w-full p-6 my-8"
            }`}
          >
            <div className="flex items-center justify-between border-b border-slate-800 pb-3 shrink-0">
              <h3 className="text-base font-bold text-white flex items-center gap-2">
                <Box className="h-4 w-4 text-indigo-400" />
                {editingCatalogItem ? "Edit Catalog Item" : "Add Catalog Item"}
              </h3>
              <div className="flex items-center gap-1.5">
                <button
                  type="button"
                  onClick={() => setIsAddModalFullscreen((prev) => !prev)}
                  aria-label={
                    isAddModalFullscreen
                      ? "Exit full screen"
                      : "Enter full screen"
                  }
                  title={
                    isAddModalFullscreen ? "Exit full screen" : "Full screen"
                  }
                  className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
                >
                  {isAddModalFullscreen ? (
                    <Minimize2 className="h-4 w-4" />
                  ) : (
                    <Maximize2 className="h-4 w-4" />
                  )}
                </button>
                <button
                  type="button"
                  onClick={() => {
                    setIsAddAppModalOpen(false)
                    setIsAddModalFullscreen(false)
                    setEditingCatalogItem(null)
                    setAppError(null)
                  }}
                  aria-label="Close modal"
                  className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
                >
                  <X className="h-5 w-5" />
                </button>
              </div>
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
                        onChange={(e) => {
                          const val = e.target.value as
                            "helm" | "kustomize" | "manifest"
                          setAppForm((prev) => ({
                            ...prev,
                            runtime: val,
                            ...(val === "manifest" && !prev.manifestsYaml
                              ? { manifestsYaml: DEFAULT_STARTER_MANIFEST }
                              : {}),
                            ...(val === "manifest" && !prev.defaultEngine
                              ? { defaultEngine: "flux" }
                              : {}),
                          }))
                        }}
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      >
                        <option value="helm">helm</option>
                        <option value="kustomize">kustomize</option>
                        <option value="manifest">
                          Raw K8s Manifest / Template
                        </option>
                      </select>
                    </div>
                  </div>

                  {appForm.runtime === "manifest" && (
                    <div className="space-y-4">
                      <div>
                        <label
                          htmlFor="app-default-engine"
                          className="block text-xs text-slate-300 font-medium mb-1"
                        >
                          Action Engine (Default) *
                        </label>
                        <select
                          id="app-default-engine"
                          data-testid="catalog-action-engine-select"
                          value={appForm.defaultEngine || "flux"}
                          onChange={(e) =>
                            setAppForm((prev) => ({
                              ...prev,
                              defaultEngine: e.target.value as
                                "flux" | "pulumi",
                            }))
                          }
                          className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                        >
                          <option value="flux">
                            Flux CD (Kustomization + postBuild substitution)
                          </option>
                          <option value="pulumi">
                            Pulumi (PKO v2 Stack + ConfigGroup preview)
                          </option>
                        </select>
                      </div>

                      <div>
                        <div className="flex items-center justify-between mb-1">
                          <label
                            htmlFor="app-manifests-yaml"
                            className="block text-xs text-slate-300 font-medium"
                          >
                            Kubernetes Manifest YAML *
                          </label>
                          <button
                            type="button"
                            data-testid="extract-manifest-vars-btn"
                            onClick={handleExtractManifestVars}
                            className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer flex items-center gap-1"
                          >
                            Auto-Extract `${`var`}` Fields
                          </button>
                        </div>
                        <textarea
                          id="app-manifests-yaml"
                          data-testid="catalog-manifests-yaml-input"
                          rows={10}
                          value={appForm.manifestsYaml ?? ""}
                          onChange={(e) =>
                            setAppForm((prev) => ({
                              ...prev,
                              manifestsYaml: e.target.value,
                            }))
                          }
                          placeholder={DEFAULT_STARTER_MANIFEST}
                          className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono text-[11px] leading-relaxed"
                        />
                      </div>
                    </div>
                  )}

                  <div>
                    <label
                      htmlFor="app-repourl"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Repository URL{" "}
                      {appForm.runtime === "manifest"
                        ? "(Optional for Raw Manifests)"
                        : "*"}
                    </label>
                    <input
                      id="app-repourl"
                      type="text"
                      required={appForm.runtime !== "manifest"}
                      placeholder={
                        appForm.runtime === "manifest"
                          ? "https://kubernetes.io/docs/concepts/overview/working-with-objects/ (optional)"
                          : "https://github.com/stefanprodan/podinfo"
                      }
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

                  <div>
                    <div className="flex items-center justify-between mb-1">
                      <label
                        htmlFor="infra-pulumi-yaml"
                        className="block text-xs text-slate-300 font-medium"
                      >
                        Pulumi.yaml Program
                      </label>
                      <button
                        type="button"
                        onClick={() => {
                          const bpId = appForm.id.trim() || "my-blueprint"
                          const bpName = appForm.name.trim() || bpId
                          const runtime = appForm.pulumiRuntime || "yaml"
                          const provider = appForm.provider || "kind"
                          const tf = appForm.terraformProvider?.trim()
                          const pkgBlock = tf
                            ? `packages:\n  ${provider}:\n    source: terraform-provider\n    parameters:\n      - ${tf}\n`
                            : ""
                          const starter = `name: ${bpId}\nruntime: ${runtime}\ndescription: ${bpName} (${provider})\n${pkgBlock}config:\n  name:\n    type: string\n    default: workload-1\nresources: {}\noutputs: {}\n`
                          setAppForm((prev) => ({
                            ...prev,
                            pulumiYaml: starter,
                          }))
                        }}
                        className="text-[11px] text-indigo-400 hover:text-indigo-300 transition-colors cursor-pointer"
                      >
                        Insert Starter Template
                      </button>
                    </div>
                    <textarea
                      id="infra-pulumi-yaml"
                      rows={10}
                      spellCheck={false}
                      placeholder={
                        editingCatalogItem
                          ? "Leave blank to preserve existing Pulumi.yaml, or paste/edit YAML..."
                          : "Optional — leave blank to auto-generate a starter Pulumi.yaml"
                      }
                      value={appForm.pulumiYaml || ""}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          pulumiYaml: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono leading-relaxed"
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
                        const selectedApp =
                          item.kind === "app"
                            ? catalog.find(
                                (c) =>
                                  c.type === "app" && c.id === item.catalogRef
                              )
                            : undefined
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
                                    canary: undefined,
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
                                onChange={(e) => {
                                  const nextRef = e.target.value
                                  const nextApp = catalog.find(
                                    (c) => c.type === "app" && c.id === nextRef
                                  )
                                  updateItem({
                                    catalogRef: nextRef,
                                    canary: nextApp?.flaggerSupported
                                      ? item.canary
                                      : undefined,
                                  })
                                }}
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
                              <div className="space-y-2">
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
                                {selectedApp?.flaggerSupported && (
                                  <label className="inline-flex items-center gap-2 text-[11px] text-slate-300 cursor-pointer select-none">
                                    <input
                                      type="checkbox"
                                      aria-label={`Step ${idx + 1} Enable Flagger Canary`}
                                      checked={!!item.canary}
                                      onChange={(e) =>
                                        updateItem({
                                          canary: e.target.checked || undefined,
                                        })
                                      }
                                      className="h-3.5 w-3.5 rounded border-slate-700 bg-slate-900 text-indigo-500 focus:ring-indigo-500 cursor-pointer"
                                    />
                                    <span>Enable Flagger Canary</span>
                                  </label>
                                )}
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
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <button
                    type="button"
                    onClick={() => {
                      if (!showSchemaEditor && !schemaJsonText) {
                        const empty = JSON.stringify(
                          { type: "object", properties: {} },
                          null,
                          2
                        )
                        setSchemaJsonText(empty)
                        setSchemaFields([])
                      }
                      setShowSchemaEditor((prev) => !prev)
                    }}
                    className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 transition-colors cursor-pointer"
                  >
                    {showSchemaEditor
                      ? "Hide JSON Schema Editor"
                      : "Edit JSON Schema (Optional)"}
                  </button>
                  {appForm.kind === "infra" && (
                    <div className="flex flex-wrap items-center gap-2">
                      {appForm.pulumiYaml?.trim() && (
                        <button
                          type="button"
                          onClick={() => {
                            const derived = deriveFieldsFromPulumiYaml(
                              appForm.pulumiYaml || ""
                            )
                            const existingSecrets = schemaFields.filter(
                              (f) =>
                                f.secret &&
                                !derived.some((d) => d.name === f.name)
                            )
                            const next = [...derived, ...existingSecrets]
                            setSchemaFields(next)
                            setSchemaJsonText(
                              fieldsToSchemaJson(next, schemaJsonText)
                            )
                            setShowSchemaEditor(true)
                          }}
                          className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer"
                        >
                          Derive from Pulumi.yaml
                        </button>
                      )}
                      {PROVIDER_CREDENTIAL_PRESETS[appForm.provider || ""] && (
                        <button
                          type="button"
                          onClick={() => {
                            const presets =
                              PROVIDER_CREDENTIAL_PRESETS[
                                appForm.provider || ""
                              ] ?? []
                            const existingNames = new Set(
                              schemaFields.map((f) => f.name)
                            )
                            const toAdd = presets.filter(
                              (p) => !existingNames.has(p.name)
                            )
                            const next = [...schemaFields, ...toAdd]
                            setSchemaFields(next)
                            setSchemaJsonText(
                              fieldsToSchemaJson(next, schemaJsonText)
                            )
                            setShowSchemaEditor(true)
                          }}
                          className="text-[11px] font-medium text-amber-400 hover:text-amber-300 transition-colors cursor-pointer"
                        >
                          + Add {appForm.provider} Credentials
                        </button>
                      )}
                    </div>
                  )}
                  {appForm.kind === "app" && appForm.runtime === "manifest" && (
                    <div className="flex flex-wrap items-center gap-2">
                      <button
                        type="button"
                        onClick={handleExtractManifestVars}
                        className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer"
                      >
                        Auto-Extract `${`var`}` Fields
                      </button>
                    </div>
                  )}
                </div>
                {showSchemaEditor && (
                  <div className="mt-2.5 space-y-3">
                    <div className="space-y-2">
                      <div className="flex items-center justify-between">
                        <span className="text-xs text-slate-300 font-medium">
                          Form Fields
                        </span>
                        <button
                          type="button"
                          onClick={() => {
                            const next: SchemaFieldDraft[] = [
                              ...schemaFields,
                              {
                                name: "",
                                title: "",
                                type: "string",
                                defaultValue: "",
                                required: false,
                                secret: false,
                              },
                            ]
                            setSchemaFields(next)
                            setSchemaJsonText(
                              fieldsToSchemaJson(next, schemaJsonText)
                            )
                          }}
                          className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 cursor-pointer"
                        >
                          + Add Field
                        </button>
                      </div>
                      {schemaFields.length > 0 && (
                        <div className="max-h-80 overflow-y-auto pr-1">
                          <SchemaFieldsEditor
                            fields={schemaFields}
                            onChange={(next) => {
                              setSchemaFields(next)
                              setSchemaJsonText(
                                fieldsToSchemaJson(next, schemaJsonText)
                              )
                            }}
                          />
                        </div>
                      )}
                    </div>
                    <div>
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
                        onChange={(e) => {
                          const text = e.target.value
                          setSchemaJsonText(text)
                          const parsed = schemaJsonToFields(text)
                          if (parsed) setSchemaFields(parsed)
                        }}
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-[11px] text-slate-100 font-mono focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      />
                    </div>
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
                      appForm.runtime !== "manifest" &&
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
