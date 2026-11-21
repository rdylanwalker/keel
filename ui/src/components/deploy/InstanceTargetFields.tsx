import React from "react"

import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { NativeSelect } from "@/components/ui/native-select"
import { CatalogItem, StackSummary, TenantSummary } from "@/types/api"
import {
  CUSTOM_CLUSTER,
  DeployTargetState,
  dedicatedNamespace,
  requiresRemoteCluster,
} from "./deployPayload"

export interface InstanceTargetFieldsProps {
  item: CatalogItem
  isManagementAddon: boolean
  tenants?: TenantSummary[]
  clusterStacks: StackSummary[]
  state: DeployTargetState
  onChange: (patch: Partial<DeployTargetState>) => void
  onTenantChange?: (tenant: string) => void
}

/** Deployment target metadata (tenant, instance name, version, cluster, namespace, canary). */
export const InstanceTargetFields: React.FC<InstanceTargetFieldsProps> = ({
  item,
  isManagementAddon,
  tenants,
  clusterStacks,
  state,
  onChange,
  onTenantChange,
}) => {
  const isApp = !isManagementAddon && item.type === "app"
  const clusterScoped = isApp && requiresRemoteCluster(item)
  const fixedNamespace = isApp ? dedicatedNamespace(item) : undefined
  return (
    <div
      className={`grid grid-cols-1 gap-3 ${isApp ? "sm:grid-cols-2" : "sm:grid-cols-1"}`}
    >
      {!isManagementAddon && tenants && tenants.length > 0 && (
        <div className="space-y-1">
          <Label htmlFor="deploy-modal-tenant-select">Target Tenant</Label>
          <NativeSelect
            id="deploy-modal-tenant-select"
            value={state.tenant}
            onChange={(e) => {
              onChange({ tenant: e.target.value })
              onTenantChange?.(e.target.value)
            }}
          >
            {tenants.map((t) => (
              <option key={t.id} value={t.id}>
                {t.id}
              </option>
            ))}
          </NativeSelect>
        </div>
      )}
      {!isManagementAddon && (
        <div className="space-y-1">
          <Label htmlFor="instance-name-input">Instance Name</Label>
          <Input
            id="instance-name-input"
            value={state.instanceName}
            onChange={(e) => onChange({ instanceName: e.target.value })}
            placeholder={item.id}
          />
        </div>
      )}
      {item.runtime === "manifest" && (
        <div className="space-y-1">
          <Label htmlFor="deploy-action-engine-select">Action Engine</Label>
          <NativeSelect
            id="deploy-action-engine-select"
            data-testid="deploy-action-engine-select"
            value={state.actionEngine}
            onChange={(e) =>
              onChange({ actionEngine: e.target.value as "flux" | "pulumi" })
            }
          >
            <option value="flux">Flux CD (Kustomization)</option>
            <option value="pulumi">Pulumi (PKO v2 Stack)</option>
          </NativeSelect>
        </div>
      )}
      {item.runtime !== "manifest" &&
        (isManagementAddon || item.type === "app") && (
          <div className="space-y-1">
            <Label htmlFor="chart-version-input">
              {item.runtime === "kustomize"
                ? "Git Ref / Version"
                : "Chart Version"}
            </Label>
            <Input
              id="chart-version-input"
              value={state.chartVersion}
              onChange={(e) => onChange({ chartVersion: e.target.value })}
              placeholder={
                item.version ||
                (item.runtime === "kustomize" ? "main" : "latest")
              }
            />
          </div>
        )}
      {isApp && (
        <>
          <div className="space-y-1">
            <Label htmlFor="cluster-select-catalog">Target Cluster</Label>
            <NativeSelect
              id="cluster-select-catalog"
              value={state.targetCluster}
              onChange={(e) => onChange({ targetCluster: e.target.value })}
              required={clusterScoped}
              aria-invalid={clusterScoped && !state.targetCluster}
              aria-describedby={
                clusterScoped ? "cluster-placement-hint" : undefined
              }
            >
              {clusterScoped ? (
                <option value="" disabled>
                  Select a workload cluster...
                </option>
              ) : (
                <option value="">Management Cluster (local)</option>
              )}
              {clusterStacks.map((s) => (
                <option key={s.id} value={`${s.name}-kubeconfig`}>
                  {s.name} ({s.tenant})
                </option>
              ))}
              <option value={CUSTOM_CLUSTER}>
                Custom kubeconfig Secret...
              </option>
            </NativeSelect>
            {state.targetCluster === CUSTOM_CLUSTER && (
              <Input
                className="mt-2"
                value={state.customClusterSecret}
                onChange={(e) =>
                  onChange({ customClusterSecret: e.target.value })
                }
                placeholder="e.g. workload-kubeconfig"
                aria-label="Custom kubeconfig Secret name"
              />
            )}
            {clusterScoped && (
              <p
                id="cluster-placement-hint"
                data-testid="cluster-placement-hint"
                className="text-[11px] text-amber-300"
              >
                Installs cluster-scoped resources (CRDs, ClusterRoles,
                webhooks): only remote workload clusters can be targeted, never
                the management cluster.
              </p>
            )}
          </div>
          <div className="space-y-1">
            <Label htmlFor="target-namespace-input">Target Namespace</Label>
            <Input
              id="target-namespace-input"
              value={
                fixedNamespace ??
                (state.targetCluster ? state.targetNamespace : "")
              }
              onChange={(e) => onChange({ targetNamespace: e.target.value })}
              disabled={!state.targetCluster || Boolean(fixedNamespace)}
              placeholder={
                state.targetCluster
                  ? "e.g. apps-prod (optional)"
                  : `tenant-${state.tenant} (local)`
              }
              title={
                fixedNamespace
                  ? `${item.name} always installs into its dedicated ${fixedNamespace} namespace`
                  : state.targetCluster
                    ? "Destination namespace on the remote workload cluster"
                    : "Management cluster apps are isolated to the tenant namespace; select a remote Target Cluster to override"
              }
            />
          </div>
          {item.flaggerSupported && (
            <div className="sm:col-span-2 flex items-start gap-2.5 bg-slate-950/70 border border-slate-800 rounded-lg px-3 py-2.5">
              <Checkbox
                id="deploy-enable-canary"
                className="mt-0.5"
                checked={state.enableCanary}
                onCheckedChange={(v) => onChange({ enableCanary: v === true })}
                aria-describedby="deploy-enable-canary-help"
              />
              <div className="text-xs text-slate-200">
                <Label
                  htmlFor="deploy-enable-canary"
                  className="font-medium text-slate-200 cursor-pointer"
                >
                  Enable Flagger Canary
                </Label>
                <span
                  id="deploy-enable-canary-help"
                  className="text-[11px] text-slate-400"
                >
                  Generates a Flagger Canary CR for progressive traffic shifting
                  via Envoy Gateway &amp; Prometheus.
                </span>
              </div>
            </div>
          )}
        </>
      )}
    </div>
  )
}
