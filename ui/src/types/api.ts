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

/** Catalog entry kind (`/api/v1/catalog/{kind}/{id}`). */
export type CatalogKind = "app" | "infra" | "bundle"

export interface CreateCatalogItemPayload {
  kind?: CatalogKind
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
  placementScope?: PlacementScope
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

export interface CatalogBundleItem {
  id: string
  kind: "infra" | "app"
  catalogRef: string
  dependsOn?: string[]
  targetClusterFrom?: string
  targetNamespace?: string
  canary?: boolean
}

/**
 * `spec.placement.scope` of a catalog application: `cluster` apps install cluster-scoped
 * resources (CRDs, ClusterRoles, webhooks) and may only target a remote workload cluster.
 */
export type PlacementScope = "namespaced" | "cluster"

export interface CatalogItem {
  id: string
  name: string
  description: string
  type: CatalogKind
  category: string
  version: string
  provider?: string
  repoUrl?: string
  chart?: string
  path?: string
  runtime?: "helm" | "kustomize" | "manifest" | string
  manifestsYaml?: string
  defaultEngine?: "flux" | "pulumi"
  /** Absent means `namespaced`. */
  placementScope?: PlacementScope
  /**
   * `spec.placement.namespace`: dedicated install namespace on the remote workload cluster
   * (`cluster` apps only); the server rejects any other `targetNamespace`.
   */
  placementNamespace?: string
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

/** Proposal scope of the platform (monorepo) repository; cluster administrators only. */
export const PLATFORM_SCOPE = "management"

/** Repository a proposal lives in (`repository` on every proposal response; never credentials). */
export interface ProposalRepository {
  /** `platform` or `tenant-<id>` (also the preview `keel.io/repo` label). */
  label: string
  /** `owner/name` of a tenant's own forge repository. */
  fullName?: string
}

export interface Proposal {
  /** PR number; unique per repository only (pair it with `scope`). */
  id: string
  title: string
  branch: string
  tenant: string
  status: "open" | "merged" | "closed"
  createdAt: string
  prUrl?: string
  /** Route scope: a tenant id (`/api/v1/tenants/{scope}/proposals/{id}`) or {@link PLATFORM_SCOPE}. */
  scope: string
  repository?: ProposalRepository
  /** Forge web page of the repository, derived from `prUrl` (http(s) forges only). */
  repoUrl?: string
  /**
   * Secret-handling decisions of the proposal that created this PR (`secretNotices` of
   * `POST /api/v1/tenants/{tenant}/proposals`): values treated as secret because of their key
   * name, or marked not secret by the user. Path and reason only, never the value.
   */
  secretNotices?: SecretNotice[]
}

/** Why a value is listed in a {@link SecretNotice} (`keel_core::SecretNoticeReason`). */
export type SecretNoticeReason = "heuristicSecret" | "markedNotSecret"

/** A secret-handling decision the user should see (`keel_core::SecretNotice`; never the value). */
export interface SecretNotice {
  /** Dot path of the value (`auth.password`; bundles prefix the item id). */
  path: string
  reason: SecretNoticeReason
  message: string
}

/** Identity of a proposal across repositories: PR ids repeat between tenant repositories. */
export interface ProposalRef {
  scope: string
  id: string
}

/** A repository whose PRs could not be listed (forge failure or rate limit; never secrets). */
export interface UnavailableRepository {
  scope: string
  repository: ProposalRepository
  reason: string
}

/** `GET /api/v1/proposals` / `GET /api/v1/tenants/{tenant}/proposals`. */
export interface ProposalListing {
  proposals: Proposal[]
  unavailable: UnavailableRepository[]
}

/** Runtime provisioning step outcome (`POST /api/v1/tenants`). */
export type TenantStepOutcome =
  "pendingNamespace" | "created" | "existed" | "rotated" | "notApplicable"

export interface TenantClusterProvisioning {
  namespaceReady: boolean
  sopsAge: TenantStepOutcome
  credential: TenantStepOutcome
}

export type BranchProtectionOutcome =
  "created" | "updated" | "unchanged" | "unsupportedPlan"

export interface ForkPolicyOutcome {
  result: "disabled" | "alreadyDisabled" | "requiresAdminSetting"
  detail?: string
}

/** Where and how a tenant's GitOps repository was provisioned. Never contains credentials. */
export interface TenantRepositoryStatus {
  /** `dedicated`: the tenant's own forge repository; `monorepo`: Local Git provider (dev only). */
  layout: "dedicated" | "monorepo"
  owner?: string
  name?: string
  /** Clone URL Flux and PKO use. */
  url?: string
  webUrl?: string
  /** `https` (repository-scoped read token) or `ssh` (read-only deploy key). */
  transport?: "https" | "ssh" | string
  created?: boolean
  mainProtection?: BranchProtectionOutcome
  forks?: ForkPolicyOutcome
  /** PR on the tenant repository adding its skeleton (absent once on `main`). */
  skeletonProposal?: Proposal
  cluster: TenantClusterProvisioning
  notes: string[]
}

/** Result of `POST /api/v1/tenants`. */
export interface CreateTenantResult {
  tenant: TenantSummary
  /** Platform-repository PR adding the tenant stub; absent when already on `main`. */
  proposal?: Proposal
  branch?: string
  repository: TenantRepositoryStatus
}

/** Result of `DELETE /api/v1/tenants/{id}`. */
export interface DeleteTenantResult {
  id: string
  proposal: Proposal
  branch: string
  /** Operator follow-ups (credential revocation, archiving the repository). */
  notes: string[]
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

/** Resource diff as sent by the server: camelCase (`resourceType`, `changedFields`). */
export interface RawResourceDiff {
  urn?: string
  name?: string
  op: ChangeOp
  resourceType?: string
  changedFields?: string[]
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
  /** Ephemeral PKO preview run state (absent for synthesized reports). */
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
  /** Server classification (always set); drives grouping and SSH availability. */
  resourceKind: InfraResourceKind
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
  /** Optional: the server omits it; rows fall back to `<namespace>-<name>`. */
  id?: string
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
  /**
   * Why the release is not Ready: its Flux object's `Ready` condition, a failed canary's
   * analysis message, or the failing tenant root Kustomization that has not applied it yet.
   */
  statusMessage?: string
  canaryWeight?: number
  canaryPhase?: string
  primaryVersion?: string
  errorRate?: number
  latencyMs?: number
  /**
   * `true` for a protected management-plane core add-on (`clusters/management/core-addons/`),
   * returned to cluster admins only with `tenant: "management"`.
   */
  managementAddon?: boolean
  /** `true` when the release cannot be edited or deleted through the API. */
  protected?: boolean
  /** `catalog/core` application id backing a management add-on. */
  catalogId?: string
  /** Flux object reconciling a management add-on. */
  addonKind?: "HelmRelease" | "Kustomization"
  /** Chart version / image tag declared in Git for a management add-on. */
  desiredVersion?: string
  /** `true` when the add-on id is defined by more than one Flux object (upgrade/rollback 409). */
  conflict?: boolean
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
  /** "Not secret" overrides persisted with the instance, when the server reports them. */
  nonSecretPaths?: string[]
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
  /**
   * Dot paths of name-heuristic secret matches the user declares not secret (stored in
   * plaintext). Omitted keeps the overrides persisted with the instance; `[]` clears them.
   */
  nonSecretPaths?: string[]
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
