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

/** Builds the `POST /api/v1/proposals` payload for a catalog deployment / edit. */
export function buildDeployProposalPayload(
  item: CatalogItem,
  target: DeployTargetState,
  values: Record<string, unknown>
): CreateProposalPayload {
  const resolvedTargetCluster =
    item.type === "app"
      ? target.targetCluster === CUSTOM_CLUSTER
        ? target.customClusterSecret.trim() || undefined
        : target.targetCluster || undefined
      : undefined
  const name = target.instanceName.trim()
  const version = target.chartVersion.trim()
  const targetNs = resolvedTargetCluster ? target.targetNamespace.trim() : ""
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
