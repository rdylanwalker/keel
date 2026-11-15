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

export interface CreateCatalogItemPayload {
  kind?: "app" | "infra" | "bundle"
  id: string
  name: string
  category: string
  description?: string
  runtime?: "helm" | "kustomize"
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
  runtime?: string
  flaggerSupported?: boolean
  pulumiRuntime?: "yaml" | "hcl" | "opentofu-bridge"
  entrypoint?: string
  protectByDefault?: boolean
  bundleItems?: CatalogBundleItem[]
  schema: RJSFSchema
  defaultValues?: Record<string, unknown>
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

export interface ResourceDiff {
  urn: string
  resource_type: string
  name: string
  op: ChangeOp
  changed_fields: string[]
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
  policyStatus?: "passed" | "failed" | "warning"
  gitDiff?: string
}

export interface StackSummary {
  id: string
  name: string
  namespace?: string
  tenant: string
  provider: "proxmox" | "aws" | "gcp" | "azure" | "hetzner" | "kind" | string
  type: "management" | "workload" | "cloud"
  status: "synced" | "reconciling" | "pending" | "error" | "drift"
  resourceCount: number
  lastSynced: string
  bundle?: string
  bundleItem?: string
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

export interface RestartStackResponse {
  restarted: boolean
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
  status: "healthy" | "progressing" | "failed" | "degraded"
  version: string
  chart?: string
  runtime?: string
  canary?: boolean
  targetCluster?: string
  bundle?: string
  bundleItem?: string
  canaryWeight?: number
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
  phase: "bootstrap" | "oidc" | "breakglass"
  username?: string
  tenant?: string
}

export interface BootstrapLoginRequest {
  token: string
}

export interface BootstrapLoginResponse {
  authenticated?: boolean
  sub?: string
  phase?: string
  success?: boolean
  token?: string
  message?: string
}

export interface ReleaseConfig {
  catalogId: string
  tenant: string
  name: string
  version?: string
  targetCluster?: string
  targetNamespace?: string
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
}
