import React, { useState } from "react"
import Form from "@rjsf/core"
import validator from "@rjsf/validator-ajv8"
import { IconButtonProps, RJSFSchema, ValidatorType } from "@rjsf/utils"
import {
  AlertCircle,
  AlertTriangle,
  ArrowDown,
  ArrowRight,
  ArrowUp,
  Copy,
  FileCode2,
  Loader2,
  Maximize2,
  Minimize2,
  Plus,
  RefreshCw,
  Trash2,
  X,
} from "lucide-react"
import {
  CatalogItem,
  Proposal,
  ReleaseSummary,
  StackSummary,
  TenantSummary,
} from "../types/api"
import {
  createProposal,
  fetchCatalogSchema,
  upgradeManagementAddon,
} from "../lib/api"
import { extractSchemaDefaults } from "../lib/schemaBuilder"
import { jsonToYaml } from "../lib/utils"

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

export interface DeployInstanceInitialConfig {
  values?: Record<string, unknown>
  name?: string
  version?: string
  tenant?: string
  targetCluster?: string
  targetNamespace?: string
  canary?: boolean
  actionEngine?: "flux" | "pulumi"
}

export interface DeployInstanceModalProps {
  isOpen: boolean
  onClose: () => void
  catalogItem: CatalogItem | null
  managementAddon?: CatalogItem | null
  tenant: string
  tenants?: TenantSummary[]
  onTenantChange?: (tenant: string) => void
  stacks?: StackSummary[]
  releases?: ReleaseSummary[]
  initialConfig?: DeployInstanceInitialConfig | null
  isEditMode?: boolean
  loadingConfig?: boolean
  configError?: string | null
  onProposalCreated?: (proposal: Proposal) => void
  onCatalogItemUpdated?: (item: CatalogItem) => void
}

export const DeployInstanceModal: React.FC<DeployInstanceModalProps> = (
  props
) => {
  if (!props.isOpen) return null
  const effectiveItem = props.managementAddon ?? props.catalogItem
  const modalKey = `${effectiveItem?.id ?? "none"}-${props.managementAddon ? "mgmt" : (props.initialConfig?.name ?? "new")}-${props.loadingConfig ? "loading" : "ready"}`
  return <DeployInstanceModalBody key={modalKey} {...props} />
}

const DeployInstanceModalBody: React.FC<DeployInstanceModalProps> = ({
  onClose,
  catalogItem,
  managementAddon = null,
  tenant,
  tenants,
  onTenantChange,
  stacks = [],
  releases = [],
  initialConfig,
  isEditMode = false,
  loadingConfig = false,
  configError = null,
  onProposalCreated,
  onCatalogItemUpdated,
}) => {
  const isManagementAddon = Boolean(managementAddon)
  const resolvedCatalogItem = managementAddon ?? catalogItem
  const [activeItem, setActiveItem] = useState<CatalogItem | null>(
    resolvedCatalogItem
  )
  const [formData, setFormData] = useState<Record<string, unknown>>(() => {
    if (!resolvedCatalogItem) return initialConfig?.values ?? {}
    const baseValues =
      initialConfig?.values ??
      (resolvedCatalogItem.defaultValues as Record<string, unknown> | undefined)
    return extractSchemaDefaults(
      resolvedCatalogItem.schema as Record<string, unknown>,
      baseValues
    )
  })
  const [instanceName, setInstanceName] = useState<string>(
    () => initialConfig?.name ?? resolvedCatalogItem?.id ?? ""
  )
  const [chartVersion, setChartVersion] = useState<string>(
    () => initialConfig?.version ?? resolvedCatalogItem?.version ?? ""
  )
  const [targetCluster, setTargetCluster] = useState<string>(() => {
    if (!initialConfig?.targetCluster) return ""
    const matchesStack = stacks.some(
      (s) => `${s.name}-kubeconfig` === initialConfig.targetCluster
    )
    return matchesStack ? initialConfig.targetCluster : "__custom__"
  })
  const [customClusterSecret, setCustomClusterSecret] = useState<string>(() => {
    if (!initialConfig?.targetCluster) return ""
    const matchesStack = stacks.some(
      (s) => `${s.name}-kubeconfig` === initialConfig.targetCluster
    )
    return matchesStack ? "" : initialConfig.targetCluster
  })
  const [targetNamespace, setTargetNamespace] = useState<string>(
    () => initialConfig?.targetNamespace ?? ""
  )
  const [enableCanary, setEnableCanary] = useState<boolean>(() =>
    initialConfig?.canary !== undefined
      ? Boolean(initialConfig.canary)
      : Boolean(catalogItem?.flaggerSupported && catalogItem?.defaultCanary)
  )
  const [localTenant, setLocalTenant] = useState<string>(
    () => initialConfig?.tenant ?? tenant
  )
  const [actionEngine, setActionEngine] = useState<"flux" | "pulumi">(() => {
    return (
      initialConfig?.actionEngine ??
      (resolvedCatalogItem?.defaultEngine as "flux" | "pulumi") ??
      "flux"
    )
  })
  const [loading, setLoading] = useState(false)
  const [syncingSchema, setSyncingSchema] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [isFullscreen, setIsFullscreen] = useState(false)

  const effectiveTenant = localTenant || tenant || "default"

  const handleFormChange = (data: { formData?: Record<string, unknown> }) => {
    if (data.formData) {
      setFormData(data.formData)
    }
  }

  const handleSyncUpstreamSchema = async () => {
    if (!activeItem?.repoUrl) return
    const chartName = activeItem.chart || activeItem.id
    setSyncingSchema(true)
    setError(null)
    try {
      const { schema: upstreamSchema } = await fetchCatalogSchema({
        repoUrl: activeItem.repoUrl,
        chart: chartName,
        version: activeItem.version,
      })
      const existingSchema = (activeItem.schema ?? {}) as Record<
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
      const updatedItem: CatalogItem = { ...activeItem, schema: mergedSchema }
      setActiveItem(updatedItem)
      onCatalogItemUpdated?.(updatedItem)
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
          : "Failed to sync schema from upstream Helm chart"
      )
    } finally {
      setSyncingSchema(false)
    }
  }

  const handleSubmit = async (
    data?: { formData?: Record<string, unknown> } | React.FormEvent
  ) => {
    if (!activeItem) return
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

    try {
      if (isManagementAddon) {
        const trimmedChartVersion = chartVersion.trim()
        const proposal = await upgradeManagementAddon(activeItem.id, {
          ...(trimmedChartVersion ? { version: trimmedChartVersion } : {}),
          values: valuesToSubmit,
        })
        onProposalCreated?.(proposal)
        onClose()
        return
      }

      const resolvedTargetCluster =
        activeItem.type === "app"
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
        Boolean(trimmedInstanceName) && trimmedInstanceName !== activeItem.id

      const proposal = await createProposal({
        catalogId: activeItem.id,
        tenant: effectiveTenant,
        values: valuesToSubmit,
        ...(isCustomName ? { name: trimmedInstanceName } : {}),
        ...(activeItem.type === "app" &&
        trimmedChartVersion &&
        trimmedChartVersion !== activeItem.version
          ? { version: trimmedChartVersion }
          : {}),
        ...(trimmedTargetNs ? { targetNamespace: trimmedTargetNs } : {}),
        title: `Deploy ${isCustomName ? trimmedInstanceName : activeItem.name} for ${effectiveTenant}`,
        kind:
          activeItem.type === "infrastructure"
            ? "infra"
            : (activeItem.type as "app" | "infra" | "bundle"),
        ...(resolvedTargetCluster
          ? { targetCluster: resolvedTargetCluster }
          : {}),
        ...(activeItem.type === "app" &&
        activeItem.flaggerSupported &&
        enableCanary
          ? { canary: true }
          : {}),
        ...(activeItem.runtime === "manifest" ? { actionEngine } : {}),
      })
      onProposalCreated?.(proposal)
      onClose()
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to create Git PR proposal"
      )
    } finally {
      setLoading(false)
    }
  }

  const yamlPreview = jsonToYaml(formData).trim()

  const effectiveInstanceName =
    instanceName.trim() || activeItem?.id || initialConfig?.name || ""
  const isExistingInstance =
    Boolean(activeItem) &&
    (activeItem!.type === "app"
      ? releases.some(
          (r) =>
            r.name === effectiveInstanceName &&
            r.namespace === `tenant-${effectiveTenant}`
        )
      : stacks.some(
          (s) =>
            s.name === effectiveInstanceName && s.tenant === effectiveTenant
        ))

  const formContent = activeItem ? (
    <>
      {activeItem.type === "bundle" &&
        activeItem.bundleItems &&
        activeItem.bundleItems.length > 0 && (
          <div className="bg-indigo-950/20 border border-indigo-500/30 rounded-xl p-3.5 space-y-2">
            <div className="text-[11px] font-semibold uppercase tracking-wider text-indigo-300">
              Bundle Execution Graph ({activeItem.bundleItems.length} items)
            </div>
            <div className="flex flex-wrap gap-2">
              {activeItem.bundleItems.map((bi) => (
                <div
                  key={bi.id}
                  className="text-[11px] font-mono bg-slate-950 border border-slate-800 rounded-lg px-2.5 py-1.5 text-slate-200 flex items-center gap-1.5"
                >
                  <span className="px-1.5 py-0.5 rounded bg-indigo-500/20 text-indigo-300 text-[10px] uppercase">
                    {bi.kind}
                  </span>
                  <span className="font-semibold text-white">{bi.id}</span>
                  <span className="text-slate-500">({bi.catalogRef})</span>
                  {bi.canary && (
                    <span className="px-1.5 py-0.5 rounded bg-emerald-500/20 text-emerald-300 text-[10px] uppercase">
                      canary
                    </span>
                  )}
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
          !isManagementAddon && activeItem.type === "app"
            ? "sm:grid-cols-2"
            : "sm:grid-cols-1"
        }`}
      >
        {!isManagementAddon && tenants && tenants.length > 0 && (
          <div>
            <label
              htmlFor="deploy-modal-tenant-select"
              className="block text-[11px] font-medium text-slate-300 mb-1"
            >
              Target Tenant
            </label>
            <select
              id="deploy-modal-tenant-select"
              value={effectiveTenant}
              onChange={(e) => {
                setLocalTenant(e.target.value)
                onTenantChange?.(e.target.value)
              }}
              className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
            >
              {tenants.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.id}
                </option>
              ))}
            </select>
          </div>
        )}
        {!isManagementAddon && (
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
              placeholder={activeItem.id}
              className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
            />
          </div>
        )}
        {activeItem.runtime === "manifest" && (
          <div>
            <label
              htmlFor="deploy-action-engine-select"
              className="block text-[11px] font-medium text-slate-300 mb-1"
            >
              Action Engine
            </label>
            <select
              id="deploy-action-engine-select"
              data-testid="deploy-action-engine-select"
              value={actionEngine}
              onChange={(e) =>
                setActionEngine(e.target.value as "flux" | "pulumi")
              }
              className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
            >
              <option value="flux">Flux CD (Kustomization)</option>
              <option value="pulumi">Pulumi (PKO v2 Stack)</option>
            </select>
          </div>
        )}
        {activeItem.runtime !== "manifest" &&
          (isManagementAddon || activeItem.type === "app") && (
            <div>
              <label
                htmlFor="chart-version-input"
                className="block text-[11px] font-medium text-slate-300 mb-1"
              >
                {activeItem.runtime === "kustomize"
                  ? "Git Ref / Version"
                  : "Chart Version"}
              </label>
              <input
                id="chart-version-input"
                type="text"
                value={chartVersion}
                onChange={(e) => setChartVersion(e.target.value)}
                placeholder={
                  activeItem.version ||
                  (activeItem.runtime === "kustomize" ? "main" : "latest")
                }
                className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
              />
            </div>
          )}
        {!isManagementAddon && activeItem.type === "app" && (
          <>
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
                <option value="__custom__">Custom kubeconfig Secret...</option>
              </select>
              {targetCluster === "__custom__" && (
                <input
                  type="text"
                  value={customClusterSecret}
                  onChange={(e) => setCustomClusterSecret(e.target.value)}
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
                    : `tenant-${effectiveTenant} (local)`
                }
                title={
                  targetCluster
                    ? "Destination namespace on the remote workload cluster"
                    : "Management cluster apps are isolated to the tenant namespace; select a remote Target Cluster to override"
                }
                className="w-full bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono disabled:opacity-50 disabled:cursor-not-allowed"
              />
            </div>
            {activeItem.flaggerSupported && (
              <div className="sm:col-span-2 flex items-start space-x-2.5 bg-slate-950/70 border border-slate-800 rounded-lg px-3 py-2.5">
                <input
                  id="deploy-enable-canary"
                  type="checkbox"
                  checked={enableCanary}
                  onChange={(e) => setEnableCanary(e.target.checked)}
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-700 bg-slate-900 text-indigo-500 focus:ring-indigo-500 cursor-pointer"
                />
                <label
                  htmlFor="deploy-enable-canary"
                  className="text-xs text-slate-200 cursor-pointer select-none"
                >
                  <span className="font-medium block">
                    Enable Flagger Canary
                  </span>
                  <span className="text-[11px] text-slate-400">
                    Generates a Flagger Canary CR for progressive traffic
                    shifting via Envoy Gateway &amp; Prometheus.
                  </span>
                </label>
              </div>
            )}
          </>
        )}
      </div>

      {!isManagementAddon && isExistingInstance && (
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
            <code className="font-mono">tenant-{effectiveTenant}</code> —
            submitting will propose an update to it. Change{" "}
            <strong>Instance Name</strong> to deploy a separate copy.
          </span>
        </div>
      )}

      {/* RJSF Form Container */}
      <div className="rjsf-container bg-slate-950 p-5 rounded-xl border border-slate-800">
        <Form
          key={activeItem.id}
          schema={activeItem.schema as RJSFSchema}
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
          <div className="mt-4 pt-4 border-t border-slate-800 flex items-center justify-end gap-2">
            <button
              type="button"
              onClick={onClose}
              className="px-4 py-2.5 bg-slate-800 hover:bg-slate-700 text-slate-300 text-xs font-medium rounded-lg transition-colors cursor-pointer"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={loading}
              className="flex items-center space-x-2 px-6 py-2.5 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-semibold rounded-lg transition-colors shadow-lg shadow-indigo-600/20 disabled:opacity-50 cursor-pointer"
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
    </>
  ) : null

  const yamlPreviewSection = (
    <div
      className={
        isFullscreen ? "flex flex-col h-full min-h-0 space-y-2" : "space-y-2"
      }
    >
      <div className="flex items-center justify-between shrink-0">
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
        className={`bg-slate-950 border border-slate-800 rounded-xl p-4 text-xs font-mono text-emerald-400 overflow-auto leading-relaxed ${
          isFullscreen ? "flex-1 min-h-0" : "max-h-56"
        }`}
      >
        {yamlPreview}
      </pre>
    </div>
  )

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={
        activeItem
          ? `${isManagementAddon ? "Upgrade" : isEditMode ? "Edit" : "Create"} ${activeItem.name}`
          : "Instance Configuration"
      }
      className={`fixed inset-0 z-50 flex items-center justify-center bg-black/70 backdrop-blur-sm ${
        isFullscreen ? "p-0" : "p-4"
      }`}
    >
      <div
        data-testid="deploy-instance-modal"
        className={`bg-slate-900 border-slate-800 flex flex-col shadow-2xl ${
          isFullscreen
            ? "w-full h-full rounded-none border-0 p-6"
            : "border rounded-2xl max-w-4xl w-full max-h-[90vh] p-6"
        }`}
      >
        {/* Modal Header */}
        <div className="flex items-start justify-between gap-4 border-b border-slate-800 pb-4 shrink-0">
          <div className="min-w-0">
            <h3 className="text-lg font-semibold text-white flex items-center gap-2 flex-wrap">
              <span>
                {activeItem
                  ? activeItem.name
                  : initialConfig?.name || "Instance Configuration"}
              </span>
              {activeItem && (
                <span className="text-xs font-normal text-slate-400">
                  ({activeItem.type})
                </span>
              )}
              {isManagementAddon && (
                <span className="text-[10px] font-semibold uppercase tracking-wider px-2 py-0.5 rounded bg-emerald-500/15 text-emerald-300 border border-emerald-500/30">
                  Management Addon
                </span>
              )}
              {!isManagementAddon && isEditMode && (
                <span className="text-[10px] font-semibold uppercase tracking-wider px-2 py-0.5 rounded bg-indigo-500/20 text-indigo-300 border border-indigo-500/30">
                  Editing Instance
                </span>
              )}
            </h3>
            {activeItem?.description && (
              <p className="text-xs text-slate-400 mt-0.5">
                {activeItem.description}
              </p>
            )}
          </div>

          <div className="flex items-center space-x-2 shrink-0">
            {(activeItem?.type === "app" || isManagementAddon) &&
              activeItem?.runtime !== "manifest" &&
              activeItem?.repoUrl && (
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
            {activeItem && (
              <span className="text-xs font-mono px-3 py-1 rounded-lg bg-indigo-500/10 text-indigo-400 border border-indigo-500/20">
                {activeItem.id}
              </span>
            )}
            <button
              type="button"
              onClick={() => setIsFullscreen((prev) => !prev)}
              aria-label={
                isFullscreen ? "Exit full screen" : "Enter full screen"
              }
              title={isFullscreen ? "Exit full screen" : "Full screen"}
              className="p-1.5 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              {isFullscreen ? (
                <Minimize2 className="h-4 w-4" />
              ) : (
                <Maximize2 className="h-4 w-4" />
              )}
            </button>
            <button
              type="button"
              onClick={onClose}
              aria-label="Close modal"
              className="p-1.5 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              <X className="h-5 w-5" />
            </button>
          </div>
        </div>

        {/* Modal Body */}
        {loadingConfig ? (
          <div className="flex-1 flex items-center justify-center py-12 text-slate-400 text-xs gap-2">
            <Loader2 className="h-4 w-4 animate-spin text-indigo-400" />
            <span>Loading instance configuration...</span>
          </div>
        ) : configError ? (
          <div className="py-6 space-y-4">
            <div
              role="alert"
              className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-start space-x-2"
            >
              <AlertCircle className="h-4 w-4 text-red-400 shrink-0 mt-0.5" />
              <span>{configError}</span>
            </div>
            <div className="flex justify-end">
              <button
                type="button"
                onClick={onClose}
                className="px-4 py-2 bg-slate-800 hover:bg-slate-700 text-slate-300 text-xs font-medium rounded-lg transition-colors"
              >
                Close
              </button>
            </div>
          </div>
        ) : !activeItem ? (
          <div className="py-8 text-center text-xs text-slate-400">
            No catalog blueprint found for this instance.
          </div>
        ) : isFullscreen ? (
          <div className="flex-1 min-h-0 mt-5 grid grid-cols-1 lg:grid-cols-2 gap-6 overflow-hidden">
            <div className="min-h-0 overflow-y-auto pr-2 space-y-5">
              {formContent}
            </div>
            <div className="min-h-0 overflow-hidden flex flex-col">
              {yamlPreviewSection}
            </div>
          </div>
        ) : (
          <div className="flex-1 min-h-0 overflow-y-auto pr-2 space-y-5 mt-5">
            {formContent}
            {yamlPreviewSection}
          </div>
        )}
      </div>
    </div>
  )
}
