import { RJSFSchema } from "@rjsf/utils"

export interface TenantSummary {
  id: string
  namespace: string
}

export interface ProviderRegistryEntry {
  id: string
  enabled: boolean
  package?: string
  description?: string
}

export type InfraResourceKind = "vm" | "cluster" | "service"

export interface CreateCatalogItemPayload {
  kind?: "app" | "infra" | "bundle"
  id: string
  name: string
  category: string
  description?: string
  protected?: boolean
  tenant?: string
  resourceKind?: InfraResourceKind
  runtime?: "helm" | "kustomize" | "manifest"
  manifestsYaml?: string
  defaultEngine?: "flux" | "pulumi"
  repoUrl?: string
  chart?: string
  version?: string
  path?: string
  flaggerSupported?: boolean
  provider?: string
  pulumiRuntime?: "yaml" | "hcl" | "opentofu-bridge"
  entrypoint?: string
  protectByDefault?: boolean
  terraformProvider?: string
  pulumiYaml?: string
  items?: CatalogBundleItem[]
  schema?: RJSFSchema
}

export type CreateCatalogAppPayload = CreateCatalogItemPayload

export interface CatalogBundleItem {
  id: string
  kind: "infra" | "app"
  catalogRef: string
  dependsOn?: string[]
  targetClusterFrom?: string
  targetNamespace?: string
  canary?: boolean
}

export interface CatalogItem {
  id: string
  name: string
  description: string
  type: "app" | "infrastructure" | "bundle" | string
  category: string
  version: string
  provider?: string
  repoUrl?: string
  chart?: string
  path?: string
  runtime?: "helm" | "kustomize" | "manifest" | string
  manifestsYaml?: string
  defaultEngine?: "flux" | "pulumi"
  flaggerSupported?: boolean
  defaultCanary?: boolean
  pulumiRuntime?: "yaml" | "hcl" | "opentofu-bridge"
  entrypoint?: string
  protectByDefault?: boolean
  pulumiYaml?: string
  bundleItems?: CatalogBundleItem[]
  serviceUrl?: string
  schema: RJSFSchema
  uiSchema?: Record<string, unknown>
  defaultValues?: Record<string, unknown>
  protected?: boolean
  tenant?: string
  resourceKind?: InfraResourceKind
}

export interface Proposal {
  id: string
  title: string
  branch: string
  author: string
  tenant: string
  status: "open" | "merged" | "closed"
  createdAt: string
  prUrl?: string
  previewStatus?: "pending" | "ready" | "failed"
}

export type ChangeOp =
  "Create" | "Update" | "Delete" | "Replace" | "Same" | "Drift"

/** Normalized resource diff consumed by the preview components. */
export interface ResourceDiff {
  urn: string
  resource_type: string
  name: string
  op: ChangeOp
  changed_fields: string[]
}

/**
 * Resource diff as sent by the server: camelCase (`resourceType`, `changedFields`). The
 * snake_case keys are accepted as a fallback for reports persisted by older servers.
 */
export interface RawResourceDiff {
  urn?: string
  name?: string
  op: ChangeOp
  resourceType?: string
  changedFields?: string[]
  resource_type?: string
  changed_fields?: string[]
}

export interface PreviewSummary {
  create: number
  update: number
  delete: number
  replace: number
  same: number
  drift: number
  total?: number
}

export interface PreviewReport {
  resources: ResourceDiff[]
  summary: PreviewSummary
  prUrl?: string
  /** Policy verdict; `undefined` means no policy evaluation was performed/reported. */
  policyStatus?: PolicyStatus
  /** `synthesized` = control-plane simulation (no PKO `pulumi preview` executed). */
  source?: PreviewSource
  /** Ephemeral PKO preview run state (absent for legacy/synthesized reports). */
  run?: PreviewRun
  gitDiff?: string
}

export type PolicyStatus = "passed" | "failed" | "warning"

export type PreviewSource = "pko" | "synthesized"

export type PreviewRunPhase =
  "pending" | "running" | "succeeded" | "failed" | "notExecuted"

/** Execution state of the ephemeral PKO v2 preview `Stack` backing a report. */
export interface PreviewRun {
  phase: PreviewRunPhase
  message?: string
  stack?: string
  commit?: string
  update?: string
}

export interface StackSummary {
  id: string
  name: string
  namespace?: string
  tenant: string
  provider: "proxmox" | "aws" | "gcp" | "azure" | "hetzner" | "kind" | string
  type: "management" | "workload" | "cloud"
  /**
   * Live/desired state. `orphaned` = live in the cluster but absent from Git; `deleting` =
   * strictly terminating. Server labels are capitalized (`Synced`, `Orphaned`, ...).
   */
  status:
    | "synced"
    | "reconciling"
    | "pending"
    | "error"
    | "drift"
    | "orphaned"
    | "deleting"
    | string
  /** `protect: true` (management plane core and protected stacks). */
  protect?: boolean
  resourceCount?: number
  lastSynced?: string
  bundle?: string
  bundleItem?: string
  resourceKind?: InfraResourceKind
  catalogId?: string
}

export interface ResourceCondition {
  type: string
  status: string
  reason: string
  message: string
  lastTransitionTime?: string
}

export interface ClusterEventSummary {
  eventType: string
  reason: string
  message: string
  count?: number
  lastSeen?: string
}

export interface ResourceMetadata {
  apiVersion?: string
  kind?: string
  uid?: string
  creationTimestamp?: string
  deletionTimestamp?: string
  labels?: Record<string, string>
  annotations?: Record<string, string>
  specSummary?: Record<string, string>
}

export interface StackDiagnostics {
  name: string
  namespace: string
  metadata?: ResourceMetadata
  conditions: ResourceCondition[]
  workspacePod?: string
  workspacePhase?: string
  events: ClusterEventSummary[]
  logs: string[]
}

export interface StackSshInfo {
  stackName: string
  namespace: string
  tenant: string
  provider: string
  sshHost: string
  sshUser: string
  sshPort: number
  sshCommand: string
  cliCommand: string
  cliExecCommand: string
  hasSavedSshKey: boolean
}

export interface RestartStackResponse {
  restarted: boolean
  namespace: string
  name: string
  message: string
}

export interface ForceDeleteStackResponse {
  forceDeleted: boolean
  namespace: string
  name: string
  message: string
}

export interface UnlockStackResponse {
  unlocked: boolean
  namespace: string
  name: string
  message: string
}

export interface ReleaseSummary {
  id: string
  name: string
  tenant: string
  namespace: string
  type: "helm" | "kustomize" | "canary"
  status: "healthy" | "progressing" | "failed" | "degraded" | string
  version: string
  chart?: string
  runtime?: string
  canary?: boolean
  targetCluster?: string
  bundle?: string
  bundleItem?: string
  lastSynced?: string
  canaryWeight?: number
  canaryPhase?: string
  primaryVersion?: string
  errorRate?: number
  latencyMs?: number
}

export type WatchedCrdKind =
  "GitRepository" | "Kustomization" | "HelmRelease" | "Stack" | "Canary"

export type CrdReconcileStatus =
  "Ready" | "Progressing" | "Failed" | "Unknown" | string

export interface CrdWatchEvent {
  kind: WatchedCrdKind
  namespace: string
  name: string
  status: CrdReconcileStatus
  message: string
  revision?: string | null
  timestamp: string
}

export interface AuthStatus {
  authenticated: boolean
  phase: "oidc-active" | "break-glass" | "oidc"
  oidcEnabled?: boolean
  sub?: string
  name?: string
  email?: string
  groups?: string[]
  isAdmin?: boolean
  username?: string
  tenant?: string
}

export interface ReleaseConfig {
  catalogId: string
  tenant: string
  name: string
  version?: string
  targetCluster?: string
  targetNamespace?: string
  canary?: boolean
  actionEngine?: "flux" | "pulumi"
  values: Record<string, unknown>
}

export interface CreateProposalPayload {
  catalogId: string
  tenant: string
  values: Record<string, unknown>
  name?: string
  version?: string
  targetNamespace?: string
  title?: string
  kind?: "app" | "infra" | "bundle"
  targetCluster?: string
  canary?: boolean
  actionEngine?: "flux" | "pulumi"
}

export interface UpgradeManagementAddonPayload {
  version?: string
  values: Record<string, unknown>
}

/** Management-plane core add-on with live Flux status (`GET /api/v1/management/addons`). */
export interface ManagementAddonStatus {
  /** Flux object name (equals the `catalog/core` application id). */
  id: string
  name: string
  namespace: string
  /** Flux object reconciling the add-on: a chart `HelmRelease` or a plain-manifest `Kustomization`. */
  kind?: "HelmRelease" | "Kustomization"
  chart?: string
  /** Chart version (`HelmRelease`) or first `spec.images[].newTag` (`Kustomization`). */
  desiredVersion?: string
  /** Live status label (`Ready`, `Reconciling`, `Error`, ...); absent when not tracked. */
  status?: string
  /** `true` when a live Flux object with the same kind/namespace/name was observed. */
  tracked: boolean
  lastSynced?: string
  /** `true` when more than one Flux object in Git is named `id` (upgrade/rollback return 409). */
  conflict?: boolean
}

export interface ManagementAddonsResponse {
  addons: ManagementAddonStatus[]
  /** `false` when the server has no live Kubernetes connection (Git-only view). */
  liveStatusAvailable: boolean
}
