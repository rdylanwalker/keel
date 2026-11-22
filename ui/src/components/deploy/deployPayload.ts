import type { RJSFSchema } from "@rjsf/utils"

import { CatalogItem, CreateProposalPayload } from "@/types/api"

export const CUSTOM_CLUSTER = "__custom__"

export interface DeployTargetState {
  tenant: string
  instanceName: string
  chartVersion: string
  targetCluster: string
  customClusterSecret: string
  targetNamespace: string
  enableCanary: boolean
  actionEngine: "flux" | "pulumi"
}

/**
 * `true` for catalog apps with `spec.placement.scope: cluster`: they install cluster-scoped
 * resources (CRDs, ClusterRoles, webhooks) and may only target a remote workload cluster.
 */
export function requiresRemoteCluster(
  item: CatalogItem | null | undefined
): boolean {
  return item?.type === "app" && item.placementScope === "cluster"
}

/**
 * Dedicated install namespace of a cluster-scoped app (`spec.placement.namespace`), which the
 * server enforces for remote installs; `undefined` when the user picks the namespace.
 */
export function dedicatedNamespace(
  item: CatalogItem | null | undefined
): string | undefined {
  return requiresRemoteCluster(item) && item?.placementNamespace
    ? item.placementNamespace
    : undefined
}

/** Remote kubeconfig Secret selected for an app, or `undefined` for the management cluster. */
export function resolveTargetCluster(
  item: CatalogItem,
  target: DeployTargetState
): string | undefined {
  if (item.type !== "app") return undefined
  const cluster =
    target.targetCluster === CUSTOM_CLUSTER
      ? target.customClusterSecret.trim()
      : target.targetCluster
  // `local` is the server-side alias of the management cluster.
  return cluster && cluster !== "local" ? cluster : undefined
}

/** Client-side mirror of the server placement rule; `null` when the target is allowed. */
export function deployPlacementError(
  item: CatalogItem,
  target: DeployTargetState
): string | null {
  if (!requiresRemoteCluster(item) || resolveTargetCluster(item, target)) {
    return null
  }
  return `${item.name} installs cluster-scoped resources (CRDs, ClusterRoles or webhooks) and can only be deployed to a remote workload cluster. Select a Target Cluster other than the management cluster.`
}

/** Builds the `POST /api/v1/tenants/{tenant}/proposals` payload for a catalog deployment / edit. */
export function buildDeployProposalPayload(
  item: CatalogItem,
  target: DeployTargetState,
  values: Record<string, unknown>
): CreateProposalPayload {
  const resolvedTargetCluster = resolveTargetCluster(item, target)
  const name = target.instanceName.trim()
  const version = target.chartVersion.trim()
  const targetNs = resolvedTargetCluster
    ? (dedicatedNamespace(item) ?? target.targetNamespace.trim())
    : ""
  const isCustomName = Boolean(name) && name !== item.id
  return {
    catalogId: item.id,
    tenant: target.tenant,
    values,
    ...(isCustomName ? { name } : {}),
    ...(item.type === "app" && version && version !== item.version
      ? { version }
      : {}),
    ...(targetNs ? { targetNamespace: targetNs } : {}),
    title: `Deploy ${isCustomName ? name : item.name} for ${target.tenant}`,
    kind:
      item.type === "infrastructure"
        ? "infra"
        : (item.type as "app" | "infra" | "bundle"),
    ...(resolvedTargetCluster ? { targetCluster: resolvedTargetCluster } : {}),
    ...(item.type === "app" && item.flaggerSupported && target.enableCanary
      ? { canary: true }
      : {}),
    ...(item.runtime === "manifest"
      ? { actionEngine: target.actionEngine }
      : {}),
  }
}

/**
 * Merges an upstream chart schema (`values.schema.json` / inferred from `values.yaml`) with the
 * catalog schema: catalog-authored properties and `required` win over upstream ones.
 */
export function mergeUpstreamSchema(
  existing: RJSFSchema | undefined,
  upstream: RJSFSchema
): RJSFSchema {
  const existingSchema = (existing ?? {}) as Record<string, unknown>
  const existingProps =
    (existingSchema.properties as Record<string, unknown> | undefined) ?? {}
  const upstreamProps =
    (upstream.properties as Record<string, unknown> | undefined) ?? {}
  return {
    ...upstream,
    required: Array.isArray(existingSchema.required)
      ? (existingSchema.required as string[])
      : (upstream.required as string[] | undefined),
    properties: {
      ...upstreamProps,
      ...existingProps,
    } as RJSFSchema["properties"],
  }
}
