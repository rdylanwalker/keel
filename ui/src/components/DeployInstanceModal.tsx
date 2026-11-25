import React, { useCallback, useMemo, useState } from "react"
import type { CustomValidator, RJSFSchema } from "@rjsf/utils"
import { AlertTriangle, ArrowRight, Loader2, RefreshCw } from "lucide-react"
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
import { errorMessage } from "../lib/errors"
import { toErrorSchema } from "../lib/formErrors"
import { validateManifestValues } from "../lib/manifestVars"
import { extractSchemaDefaults } from "../lib/schemaBuilder"
import {
  withBundleCanaryHostsRequired,
  withCanaryHostRequired,
} from "../lib/canaryHost"
import { jsonToYaml } from "../lib/utils"
import {
  buildSecretAwareUiSchema,
  HeuristicSecretField,
  heuristicSecretCandidates,
  heuristicSecretSchemaPaths,
  isBlankValue,
  maskSecretValues,
  maskValuesAtPaths,
  relaxRequiredAtPaths,
  relaxSecretRequirements,
  valueAtDotPath,
  withHeuristicSecretUi,
} from "../lib/secretFields"
import { SchemaResolver, splitDotPath } from "../lib/schemaResolver"
import { Button } from "./ui/button"
import { Badge } from "./ui/badge"
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogFullscreenToggle,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "./ui/dialog"
import { SchemaForm } from "./rjsf/SchemaForm"
import { InlineAlert } from "./catalog/InlineAlert"
import { BundleExecutionGraph } from "./deploy/BundleExecutionGraph"
import { InstanceTargetFields } from "./deploy/InstanceTargetFields"
import { ValuesYamlPreview } from "./deploy/ValuesYamlPreview"
import {
  buildDeployProposalPayload,
  CUSTOM_CLUSTER,
  DeployTargetState,
  deployPlacementError,
  mergeUpstreamSchema,
  requiresRemoteCluster,
} from "./deploy/deployPayload"
import { serverFieldErrors } from "./deploy/serverFieldErrors"
import { HeuristicSecretSummary } from "./deploy/HeuristicSecretSummary"

type Values = Record<string, unknown>

export interface DeployInstanceInitialConfig {
  values?: Record<string, unknown>
  name?: string
  version?: string
  tenant?: string
  targetCluster?: string
  targetNamespace?: string
  canary?: boolean
  actionEngine?: "flux" | "pulumi"
  /** "Not secret" overrides persisted with the instance (when the config endpoint reports them). */
  nonSecretPaths?: string[]
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

/**
 * Workload-cluster stacks usable by `tenant`: a remote install's `kubeConfig.secretRef` resolves in
 * the release's own `tenant-<id>` namespace (Flux `--no-cross-namespace-refs`), so another
 * tenant's `<stack>-kubeconfig` Secret can never be referenced.
 */
function stacksForTenant(
  stacks: StackSummary[],
  tenant: string
): StackSummary[] {
  return stacks.filter((s) => !s.tenant || s.tenant === tenant)
}

function initialTargetState(
  props: DeployInstanceModalProps,
  item: CatalogItem | null,
  clusterStacks: StackSummary[]
): DeployTargetState {
  const { initialConfig, tenant, catalogItem } = props
  const initialTenant = initialConfig?.tenant ?? tenant
  const tenantStacks = stacksForTenant(
    clusterStacks,
    initialTenant || "default"
  )
  const cluster = initialConfig?.targetCluster
  const matchesStack =
    Boolean(cluster) &&
    tenantStacks.some((s) => `${s.name}-kubeconfig` === cluster)
  // Cluster-scoped apps cannot target the management cluster: preselect the tenant's first
  // workload cluster for new deployments only. Edits never silently retarget an existing release.
  const defaultCluster =
    requiresRemoteCluster(item) &&
    !props.managementAddon &&
    !initialConfig &&
    tenantStacks[0]
      ? `${tenantStacks[0].name}-kubeconfig`
      : ""
  return {
    tenant: initialTenant,
    instanceName: initialConfig?.name ?? item?.id ?? "",
    chartVersion: initialConfig?.version ?? item?.version ?? "",
    targetCluster: !cluster
      ? defaultCluster
      : matchesStack
        ? cluster
        : CUSTOM_CLUSTER,
    customClusterSecret: cluster && !matchesStack ? cluster : "",
    targetNamespace: initialConfig?.targetNamespace ?? "",
    enableCanary:
      initialConfig?.canary !== undefined
        ? Boolean(initialConfig.canary)
        : Boolean(catalogItem?.flaggerSupported && catalogItem?.defaultCanary),
    actionEngine:
      initialConfig?.actionEngine ??
      (item?.defaultEngine as "flux" | "pulumi" | undefined) ??
      "flux",
  }
}

/**
 * "Not secret" overrides an edit starts from: the persisted list when the config endpoint
 * reports it, otherwise every heuristic match present in the loaded plaintext values (a
 * heuristic secret is never in `values.yaml`, so a plaintext match was marked not secret).
 */
function initialNotSecretPaths(
  schema: unknown,
  initialConfig: DeployInstanceInitialConfig | null | undefined,
  editMode: boolean
): string[] {
  if (initialConfig?.nonSecretPaths) return [...initialConfig.nonSecretPaths]
  if (!editMode || !initialConfig?.values) return []
  return heuristicSecretCandidates(schema ?? {}, initialConfig.values).map(
    (c) => c.path
  )
}

/** Whether every segment of the dot `path` is a named `properties` entry (rendered inline). */
function isSchemaNamedPath(schema: unknown, path: string): boolean {
  const resolver = new SchemaResolver(schema ?? {})
  const segments = splitDotPath(path)
  if (segments.length === 0 || segments[0] === "[]") return false
  let set: unknown[] = [schema ?? {}]
  for (const segment of segments) {
    if (segment === "[]") {
      set = resolver.itemSchemas(set)
      continue
    }
    const named = resolver
      .expand(set)
      .some(
        (n) =>
          n.properties !== null &&
          typeof n.properties === "object" &&
          Object.prototype.hasOwnProperty.call(n.properties, segment)
      )
    if (!named) return false
    set = resolver.propertySchemas(set, segment)
  }
  return true
}

const DeployInstanceModalBody: React.FC<DeployInstanceModalProps> = (props) => {
  const {
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
  } = props
  const isManagementAddon = Boolean(managementAddon)
  const resolvedCatalogItem = managementAddon ?? catalogItem
  const [activeItem, setActiveItem] = useState<CatalogItem | null>(
    resolvedCatalogItem
  )
  const [formData, setFormData] = useState<Values>(() => {
    if (!resolvedCatalogItem) return initialConfig?.values ?? {}
    const baseValues =
      initialConfig?.values ??
      (resolvedCatalogItem.defaultValues as Values | undefined)
    return extractSchemaDefaults(
      resolvedCatalogItem.schema as Values,
      baseValues
    )
  })
  const clusterStacks = stacks.filter(
    (s) => s.resourceKind === "cluster" && s.name !== "management-cluster-core"
  )
  const [target, setTarget] = useState<DeployTargetState>(() =>
    initialTargetState(props, resolvedCatalogItem, clusterStacks)
  )
  const [loading, setLoading] = useState(false)
  const [syncingSchema, setSyncingSchema] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [fieldErrors, setFieldErrors] = useState<Record<string, string[]>>({})
  const [isFullscreen, setIsFullscreen] = useState(false)
  // Name-heuristic secrets (AST-2): values whose key looks like a secret are stored encrypted
  // unless the user marks them not secret. Management add-ons use their own upgrade route.
  const heuristicEnabled = !isManagementAddon
  // The overrides the edit started from (persisted with the instance).
  const [startNotSecretPaths] = useState<string[]>(() =>
    initialNotSecretPaths(
      resolvedCatalogItem?.schema,
      initialConfig,
      isEditMode && heuristicEnabled
    )
  )
  const [notSecretPaths, setNotSecretPaths] =
    useState<string[]>(startNotSecretPaths)
  const [overridesTouched, setOverridesTouched] = useState(false)

  const effectiveTenant = target.tenant || tenant || "default"
  const effectiveTarget = { ...target, tenant: effectiveTenant }
  const tenantClusterStacks = stacksForTenant(clusterStacks, effectiveTenant)
  const handleTargetChange = (patch: Partial<DeployTargetState>) =>
    setTarget((prev) => {
      const next = { ...prev, ...patch }
      // A tenant switch drops a selected workload cluster that belongs to another tenant.
      if (
        patch.tenant !== undefined &&
        next.targetCluster &&
        next.targetCluster !== CUSTOM_CLUSTER &&
        !stacksForTenant(clusterStacks, next.tenant || "default").some(
          (s) => `${s.name}-kubeconfig` === next.targetCluster
        )
      ) {
        next.targetCluster = ""
      }
      return next
    })

  // Canaries need a hostname (a host-less Flagger route would catch every host on the shared
  // Gateway), so the form requires `ingressHost` while the canary is enabled.
  const canaryEnabled =
    !isManagementAddon &&
    activeItem?.type === "app" &&
    Boolean(activeItem.flaggerSupported) &&
    target.enableCanary
  const itemSchema = activeItem?.schema
  const itemType = activeItem?.type
  const bundleItems = activeItem?.bundleItems
  // Edit: app (Helm and manifest) and bundle secrets left blank keep their stored values, so
  // they are optional and hint so. Infrastructure stacks re-encrypt only the submitted
  // secrets: their secret fields stay required and must be re-entered.
  const keepsStoredSecrets = isEditMode && itemType !== "infra"
  const schemaHeuristicPaths = useMemo(
    () => (heuristicEnabled ? heuristicSecretSchemaPaths(itemSchema) : []),
    [heuristicEnabled, itemSchema]
  )
  const valueHeuristicPaths = useMemo(
    () =>
      heuristicEnabled
        ? heuristicSecretCandidates(itemSchema ?? {}, formData).map(
            (c) => c.path
          )
        : [],
    [heuristicEnabled, itemSchema, formData]
  )
  const heuristicFields = useMemo<HeuristicSecretField[]>(() => {
    const paths = [
      ...new Set([...schemaHeuristicPaths, ...valueHeuristicPaths]),
    ].sort()
    return paths.map((path) => {
      const notSecret = notSecretPaths.includes(path)
      return {
        path,
        notSecret,
        fromSchema: schemaHeuristicPaths.includes(path),
        // A stored heuristic secret opens blank: marking it not secret needs the value again.
        reenter:
          keepsStoredSecrets &&
          notSecret &&
          !startNotSecretPaths.includes(path) &&
          isBlankValue(valueAtDotPath(formData, path)),
      }
    })
  }, [
    schemaHeuristicPaths,
    valueHeuristicPaths,
    notSecretPaths,
    startNotSecretPaths,
    keepsStoredSecrets,
    formData,
  ])
  // Matches the form cannot show inline (keys added under additionalProperties).
  const unnamedHeuristicFields = useMemo(
    () => heuristicFields.filter((f) => !isSchemaNamedPath(itemSchema, f.path)),
    [heuristicFields, itemSchema]
  )
  const relaxedHeuristicPaths = useMemo(
    () =>
      keepsStoredSecrets
        ? schemaHeuristicPaths.filter((p) => !notSecretPaths.includes(p))
        : [],
    [keepsStoredSecrets, schemaHeuristicPaths, notSecretPaths]
  )
  const formSchema = useMemo(() => {
    const base = keepsStoredSecrets
      ? relaxRequiredAtPaths(
          relaxSecretRequirements(itemSchema),
          relaxedHeuristicPaths
        )
      : itemSchema
    const withHosts =
      itemType === "bundle"
        ? withBundleCanaryHostsRequired(base, bundleItems)
        : withCanaryHostRequired(base, canaryEnabled)
    return withHosts as RJSFSchema | undefined
  }, [
    itemSchema,
    itemType,
    bundleItems,
    keepsStoredSecrets,
    canaryEnabled,
    relaxedHeuristicPaths,
  ])
  const formUiSchema = useMemo(
    () =>
      withHeuristicSecretUi(
        buildSecretAwareUiSchema(activeItem?.schema, activeItem?.uiSchema, {
          editMode: keepsStoredSecrets,
        }),
        heuristicFields,
        { editMode: keepsStoredSecrets }
      ),
    [
      activeItem?.schema,
      activeItem?.uiSchema,
      keepsStoredSecrets,
      heuristicFields,
    ]
  )
  const handleToggleNotSecret = useCallback(
    (path: string, notSecret: boolean) => {
      setOverridesTouched(true)
      setNotSecretPaths((prev) =>
        notSecret
          ? prev.includes(path)
            ? prev
            : [...prev, path].sort()
          : prev.filter((p) => p !== path)
      )
    },
    []
  )
  const formContext = useMemo(
    () => ({ onToggleNotSecret: handleToggleNotSecret }),
    [handleToggleNotSecret]
  )
  const manifestTemplate =
    activeItem?.runtime === "manifest" ? activeItem.manifestsYaml : undefined
  const customValidate = useMemo<
    CustomValidator<Values, RJSFSchema> | undefined
  >(() => {
    if (!manifestTemplate) return undefined
    return (data, errors) => {
      const problems = validateManifestValues(manifestTemplate, data)
      for (const [name, messages] of Object.entries(problems)) {
        const slot = (errors as Record<string, unknown>)[name] as
          { addError?: (m: string) => void } | undefined
        messages.forEach((m) => slot?.addError?.(m))
      }
      return errors
    }
  }, [manifestTemplate])

  const handleSyncUpstreamSchema = async () => {
    if (!activeItem?.repoUrl) return
    setSyncingSchema(true)
    setError(null)
    try {
      const { schema: upstreamSchema } = await fetchCatalogSchema({
        repoUrl: activeItem.repoUrl,
        chart: activeItem.chart || activeItem.id,
        version: activeItem.version,
      })
      const mergedSchema = mergeUpstreamSchema(
        activeItem.schema,
        upstreamSchema
      )
      const updatedItem: CatalogItem = { ...activeItem, schema: mergedSchema }
      setActiveItem(updatedItem)
      onCatalogItemUpdated?.(updatedItem)
      setFormData(
        extractSchemaDefaults(
          mergedSchema as Values,
          updatedItem.defaultValues as Values | undefined
        )
      )
    } catch (err: unknown) {
      setError(
        errorMessage(err, "Failed to sync schema from upstream Helm chart")
      )
    } finally {
      setSyncingSchema(false)
    }
  }

  const handleSubmit = async (values: Values) => {
    if (!activeItem) return
    setLoading(true)
    setError(null)
    setFieldErrors({})
    try {
      if (isManagementAddon) {
        const version = target.chartVersion.trim()
        const proposal = await upgradeManagementAddon(activeItem.id, {
          ...(version ? { version } : {}),
          values,
        })
        onProposalCreated?.(proposal)
        onClose()
        return
      }
      const placementError = deployPlacementError(activeItem, effectiveTarget)
      if (placementError) {
        setError(placementError)
        return
      }
      // New instances send the overrides that apply; edits send them only once the user
      // changed one (omitted keeps the overrides persisted with the instance).
      const flagged = new Set(heuristicFields.map((f) => f.path))
      const overrides = notSecretPaths.filter((p) => flagged.has(p))
      const sendOverrides = isEditMode ? overridesTouched : overrides.length > 0
      const proposal = await createProposal(
        buildDeployProposalPayload(
          activeItem,
          effectiveTarget,
          values,
          sendOverrides ? overrides : undefined
        )
      )
      onProposalCreated?.(proposal)
      onClose()
    } catch (err) {
      setError(errorMessage(err, "Failed to create Git PR proposal"))
      setFieldErrors(serverFieldErrors(err))
    } finally {
      setLoading(false)
    }
  }

  // Secret (writeOnly / format: password, and name-heuristic matches not marked "not a
  // secret") values are masked in the on-screen preview.
  const yamlPreview = jsonToYaml(
    maskValuesAtPaths(
      maskSecretValues(activeItem?.schema, formData),
      valueHeuristicPaths.filter((p) => !notSecretPaths.includes(p))
    )
  ).trim()

  const effectiveInstanceName =
    target.instanceName.trim() || activeItem?.id || initialConfig?.name || ""
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

  const verb = isManagementAddon || isEditMode ? "Edit" : "Create"
  const canSyncSchema =
    (activeItem?.type === "app" || isManagementAddon) &&
    activeItem?.runtime !== "manifest" &&
    Boolean(activeItem?.repoUrl)

  const formContent = activeItem ? (
    <>
      {activeItem.type === "bundle" &&
        activeItem.bundleItems &&
        activeItem.bundleItems.length > 0 && (
          <BundleExecutionGraph items={activeItem.bundleItems} />
        )}

      <InstanceTargetFields
        item={activeItem}
        isManagementAddon={isManagementAddon}
        tenants={tenants}
        clusterStacks={tenantClusterStacks}
        state={effectiveTarget}
        onChange={handleTargetChange}
        onTenantChange={onTenantChange}
      />

      {!isManagementAddon && isExistingInstance && (
        <div
          role="status"
          className="flex items-start gap-2 p-3 rounded-xl bg-amber-950/30 border border-amber-500/30 text-amber-200 text-xs"
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

      {unnamedHeuristicFields.length > 0 && (
        <HeuristicSecretSummary
          fields={unnamedHeuristicFields}
          onToggle={handleToggleNotSecret}
        />
      )}

      <div className="bg-slate-950 p-4 sm:p-5 rounded-xl border border-slate-800">
        <SchemaForm
          key={activeItem.id}
          schema={formSchema as RJSFSchema}
          uiSchema={formUiSchema}
          formData={formData}
          onChange={setFormData}
          onSubmit={handleSubmit}
          customValidate={customValidate}
          extraErrors={toErrorSchema(fieldErrors)}
          formContext={formContext}
        >
          <div className="mt-4 pt-4 border-t border-slate-800 flex flex-col-reverse sm:flex-row sm:items-center sm:justify-end gap-2">
            <Button variant="secondary" size="lg" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" size="lg" disabled={loading}>
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
            </Button>
          </div>
        </SchemaForm>
      </div>

      {error && <InlineAlert>{error}</InlineAlert>}
    </>
  ) : null

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        size="xl"
        closeLabel="Close modal"
        data-testid="deploy-instance-modal"
        aria-describedby={
          activeItem?.description ? "deploy-modal-description" : undefined
        }
        className={
          isFullscreen
            ? "sm:max-w-none w-screen h-dvh max-h-dvh rounded-none border-0"
            : "sm:max-h-[90vh]"
        }
        controls={
          <DialogFullscreenToggle
            fullscreen={isFullscreen}
            onToggle={() => setIsFullscreen((prev) => !prev)}
          />
        }
      >
        <DialogHeader>
          <DialogTitle>
            <span className="text-slate-400 font-medium">{verb}</span>{" "}
            {activeItem
              ? activeItem.name
              : initialConfig?.name || "Instance Configuration"}
          </DialogTitle>
          <div className="flex flex-wrap items-center gap-2">
            {activeItem && (
              <Badge variant="secondary" className="font-mono">
                {activeItem.type}
              </Badge>
            )}
            {activeItem && <Badge className="font-mono">{activeItem.id}</Badge>}
            {isManagementAddon && (
              <Badge variant="success" className="uppercase tracking-wider">
                Management Addon
              </Badge>
            )}
            {!isManagementAddon && isEditMode && (
              <Badge className="uppercase tracking-wider">
                Editing Instance
              </Badge>
            )}
            {canSyncSchema && (
              <div className="ml-auto flex items-center gap-1.5">
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={handleSyncUpstreamSchema}
                  disabled={syncingSchema}
                  title="Pull values.schema.json or values.yaml from upstream Helm chart"
                >
                  <RefreshCw
                    className={`h-3 w-3 ${syncingSchema ? "animate-spin" : ""}`}
                  />
                  <span>
                    {syncingSchema ? "Syncing..." : "Sync Upstream Schema"}
                  </span>
                </Button>
              </div>
            )}
          </div>
          {activeItem?.description && (
            <DialogDescription id="deploy-modal-description">
              {activeItem.description}
            </DialogDescription>
          )}
        </DialogHeader>

        {loadingConfig ? (
          <div className="flex-1 flex items-center justify-center py-12 text-slate-400 text-xs gap-2">
            <Loader2 className="h-4 w-4 animate-spin text-indigo-400" />
            <span>Loading instance configuration...</span>
          </div>
        ) : configError ? (
          <DialogBody className="space-y-4">
            <InlineAlert>{configError}</InlineAlert>
            <div className="flex justify-end">
              <Button variant="secondary" onClick={onClose}>
                Close
              </Button>
            </div>
          </DialogBody>
        ) : !activeItem ? (
          <div className="py-8 text-center text-xs text-slate-400">
            No catalog blueprint found for this instance.
          </div>
        ) : isFullscreen ? (
          <div className="flex-1 min-h-0 p-6 grid grid-cols-1 lg:grid-cols-2 gap-6 overflow-hidden">
            <div className="min-h-0 overflow-y-auto pr-2 space-y-5">
              {formContent}
            </div>
            <div className="min-h-0 overflow-hidden flex flex-col">
              <ValuesYamlPreview yaml={yamlPreview} fullscreen />
            </div>
          </div>
        ) : (
          <DialogBody className="space-y-5">
            {formContent}
            <ValuesYamlPreview yaml={yamlPreview} fullscreen={false} />
          </DialogBody>
        )}
      </DialogContent>
    </Dialog>
  )
}
