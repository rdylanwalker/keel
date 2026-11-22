import React, { useCallback, useMemo, useState } from "react"
import {
  Activity,
  AlertTriangle,
  Boxes,
  Gauge,
  Package,
  RefreshCw,
} from "lucide-react"

import {
  InfraResourceKind,
  Proposal,
  ProposalRef,
  ReleaseSummary,
  StackSummary,
} from "../types/api"
import {
  deleteRelease,
  deleteStack,
  reconcileRelease,
  reconcileStack,
} from "../lib/api"
import { errorMessage } from "../lib/errors"
import { isDemoMode } from "../lib/utils"
import { useAuthz } from "../lib/authz-context"
import { CatalogKindFilter } from "../lib/routes"
import { StackInspectorModal } from "./StackInspectorModal"
import { PrNotificationBanner } from "./PrNotificationBanner"
import { DataTable } from "./DataTable"
import { CanaryTrafficSection } from "./CanaryTrafficSection"
import { TelemetryStreamPanel } from "./TelemetryStreamPanel"
import { Button } from "./ui/button"
import { PageHeader } from "./ui/page-header"
import { useTopologyData } from "./topology/useTopologyData"
import {
  buildReleaseColumns,
  buildStackColumns,
  isManagementStack,
  ReleaseRowBusyContext,
  releaseRowId,
  releaseRowProps,
  sortManagementFirst,
  stackNamespace,
} from "./topology/columns"
import { TopologySection } from "./topology/TopologySection"
import { TopologyStats } from "./topology/TopologyStats"
import { ManagementPlaneStrip } from "./topology/ManagementPlaneStrip"
import { ManagementAddonDialogs } from "./topology/ManagementAddonDialogs"
import { useManagementAddonActions } from "./topology/useManagementAddonActions"
import { StreamStatus } from "./topology/StreamStatus"

// Load the SSH terminal (wterm WASM core) only when a session is opened.
const VmSshModal = React.lazy(() =>
  import("./VmSshModal").then((m) => ({ default: m.VmSshModal }))
)

export interface TopologyViewProps {
  /** Navigates to the catalog, optionally pre-filtered by kind (`#catalog/<kind>`). */
  onNavigateToCatalog?: (kind?: CatalogKindFilter) => void
  onNavigateToPreview?: (ref?: ProposalRef) => void
  onNavigateToProviders?: () => void
  onEditRelease?: (namespace: string, name: string) => void
  onEditStack?: (namespace: string, name: string) => void
  authenticated?: boolean
  selectedTenant?: string
}

interface InspectTarget {
  namespace: string
  name: string
  kind?: "stack" | "release"
  tenant?: string
  provider?: string
  resourceKind?: InfraResourceKind
}

interface SshTarget {
  stackName: string
  namespace: string
  tenant: string
  provider?: string
}

interface Banner {
  message: string
  prUrl?: string
  prId?: string
  prScope?: string
}

export const TopologyView: React.FC<TopologyViewProps> = ({
  onNavigateToCatalog,
  onNavigateToPreview,
  onNavigateToProviders,
  onEditRelease,
  onEditStack,
  authenticated = true,
  selectedTenant,
}) => {
  const authz = useAuthz()
  const data = useTopologyData(authenticated, selectedTenant, authz.isAdmin)
  const { stacks, releases, setError, scheduleRefresh } = data

  const [inspected, setInspected] = useState<InspectTarget | null>(null)
  const [sshStack, setSshStack] = useState<SshTarget | null>(null)
  const [reconcilingStackKey, setReconcilingStackKey] = useState<string | null>(
    null
  )
  const [reconcilingReleaseKey, setReconcilingReleaseKey] = useState<
    string | null
  >(null)
  const [deletingKey, setDeletingKey] = useState<string | null>(null)
  const [stackBanner, setStackBanner] = useState<Banner | null>(null)
  const [releaseBanner, setReleaseBanner] = useState<Banner | null>(null)
  const [eventFilterKind, setEventFilterKind] = useState<string>("all")

  const stackColumns = useMemo(() => {
    const actions = {
      authz,
      onEdit: onEditStack,
      reconcilingKey: reconcilingStackKey,
      deletingKey,
      onSsh: (stack: StackSummary, ns: string) =>
        setSshStack({
          stackName: stack.name,
          namespace: ns,
          tenant: stack.tenant || "default",
          provider: stack.provider,
        }),
      onInspect: (stack: StackSummary, ns: string) =>
        setInspected({
          namespace: ns,
          name: stack.name,
          tenant: stack.tenant,
          provider: stack.provider,
          resourceKind: stack.resourceKind,
        }),
      onReconcile: async (stack: StackSummary) => {
        const ns = stackNamespace(stack)
        setReconcilingStackKey(`${ns}/${stack.name}`)
        setStackBanner(null)
        try {
          const res = await reconcileStack(ns, stack.name)
          setStackBanner({ message: res.message })
          scheduleRefresh(1500)
        } catch (err) {
          setError(errorMessage(err, "Failed to reconcile stack"))
        } finally {
          setReconcilingStackKey(null)
        }
      },
      onDelete: async (stack: StackSummary) => {
        const ns = stackNamespace(stack)
        setDeletingKey(`stack:${ns}/${stack.name}`)
        setStackBanner(null)
        setError(null)
        try {
          const res = await deleteStack(ns, stack.name)
          setStackBanner({
            message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for stack ${stack.name}`,
            prUrl: res.proposal.prUrl,
            prId: res.proposal.id,
            prScope: res.proposal.scope,
          })
          scheduleRefresh(1000)
        } catch (err) {
          setError(errorMessage(err, "Failed to open delete stack PR"))
        } finally {
          setDeletingKey(null)
        }
      },
    }
    return {
      cluster: buildStackColumns("Cluster Name", false, actions),
      vm: buildStackColumns("Stack Name", true, actions),
      service: buildStackColumns("Service Name", false, actions),
    }
  }, [
    authz,
    onEditStack,
    reconcilingStackKey,
    deletingKey,
    scheduleRefresh,
    setError,
  ])

  const handleReconcileRelease = useCallback(
    async (rel: ReleaseSummary) => {
      const ns = rel.namespace || "tenant-default"
      setReconcilingReleaseKey(`${ns}/${rel.name}`)
      setReleaseBanner(null)
      try {
        const res = await reconcileRelease(ns, rel.name)
        setReleaseBanner({ message: res.message })
        scheduleRefresh(1500)
      } catch (err) {
        setError(errorMessage(err, "Failed to reconcile release"))
      } finally {
        setReconcilingReleaseKey(null)
      }
    },
    [scheduleRefresh, setError]
  )

  const handleAddonProposal = useCallback(
    (proposal: Proposal, action: "upgrade" | "rollback", id: string) => {
      setReleaseBanner({
        message: `Opened ${action === "upgrade" ? "Edit" : "Rollback"} PR #${proposal.id} (${proposal.branch || "main"}) for management add-on ${id}`,
        prUrl: proposal.prUrl,
        prId: proposal.id,
        prScope: proposal.scope,
      })
    },
    []
  )

  const demo = isDemoMode()
  // Owned here (not by table rows) so an open upgrade form or rollback confirmation survives a
  // failed or empty releases refresh; catalog/core is fetched once, for authenticated admins.
  const addonActions = useManagementAddonActions({
    enabled: authz.isAdmin && authenticated,
    demo,
    onProposal: handleAddonProposal,
    onError: setError,
  })
  const {
    catalogById: addonCatalogById,
    rollingBackId: addonRollingBackId,
    openUpgrade: openAddonUpgrade,
    requestRollback: requestAddonRollback,
  } = addonActions

  const releaseColumns = useMemo(
    () =>
      buildReleaseColumns({
        authz,
        onEdit: onEditRelease,
        onInspect: (rel: ReleaseSummary, ns: string) =>
          setInspected({ namespace: ns, name: rel.name, kind: "release" }),
        onReconcile: handleReconcileRelease,
        management: authz.isAdmin
          ? {
              catalogById: addonCatalogById,
              onUpgrade: openAddonUpgrade,
              onRollback: requestAddonRollback,
            }
          : undefined,
        onDelete: async (rel: ReleaseSummary) => {
          const ns = rel.namespace || "tenant-default"
          setDeletingKey(`release:${ns}/${rel.name}`)
          setReleaseBanner(null)
          setError(null)
          try {
            const res = await deleteRelease(ns, rel.name)
            setReleaseBanner({
              message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for release ${rel.name}`,
              prUrl: res.proposal.prUrl,
              prId: res.proposal.id,
              prScope: res.proposal.scope,
            })
            scheduleRefresh(1000)
          } catch (err) {
            setError(errorMessage(err, "Failed to open delete release PR"))
          } finally {
            setDeletingKey(null)
          }
        },
      }),
    [
      authz,
      onEditRelease,
      handleReconcileRelease,
      addonCatalogById,
      openAddonUpgrade,
      requestAddonRollback,
      scheduleRefresh,
      setError,
    ]
  )

  // Management add-ons (admin-only from the API) are protected rows of the releases table,
  // listed first; their actions never include Edit or Delete.
  const { tableReleases, tenantReleases, hasManagementAddons } = useMemo(() => {
    const tenantReleases = releases.filter((r) => !r.managementAddon)
    const tableReleases = authz.isAdmin
      ? sortManagementFirst(releases)
      : tenantReleases
    return {
      tableReleases,
      tenantReleases,
      hasManagementAddons: tableReleases.length > tenantReleases.length,
    }
  }, [releases, authz.isAdmin])

  const managementStack = stacks.find(isManagementStack)
  const byKind = useMemo(() => {
    const groups = {
      cluster: [] as StackSummary[],
      vm: [] as StackSummary[],
      service: [] as StackSummary[],
    }
    for (const s of stacks) groups[s.resourceKind ?? "vm"].push(s)
    return groups
  }, [stacks])

  const canaries = tenantReleases.filter(
    (r) => r.canary || r.type === "canary" || r.canaryWeight !== undefined
  )
  const filteredEvents =
    eventFilterKind === "all"
      ? data.events
      : data.events.filter((e) => e.kind === eventFilterKind)

  const deployButton = (kind: CatalogKindFilter, label: string) =>
    onNavigateToCatalog ? (
      <Button
        variant="subtle"
        onClick={() => onNavigateToCatalog(kind)}
        className="self-start sm:self-auto"
      >
        {label}
      </Button>
    ) : undefined

  const releaseBusy = useMemo(
    () => ({
      reconcilingKey: reconcilingReleaseKey,
      deletingKey,
      rollingBackId: addonRollingBackId,
    }),
    [reconcilingReleaseKey, deletingKey, addonRollingBackId]
  )

  const rowId = (row: StackSummary) => row.id || `${row.namespace}-${row.name}`

  return (
    <div className="space-y-6">
      <PageHeader
        icon={<Boxes className="h-6 w-6 text-indigo-400" />}
        title="Cluster & Workload Topology"
        description="Monitor provisioned workload stacks, application releases, canary rollouts, and live CRD events"
        actions={
          <>
            <StreamStatus
              state={data.streamState}
              pollIntervalMs={data.pollIntervalMs}
            />
            <Button
              variant="secondary"
              onClick={() => void data.refresh()}
              disabled={data.loading}
            >
              <RefreshCw
                className={`h-3.5 w-3.5 ${data.loading ? "animate-spin" : ""}`}
              />
              <span>Refresh Map</span>
            </Button>
          </>
        }
      />

      {data.error && authenticated && (
        <div
          role="alert"
          className="p-4 bg-amber-950/40 border border-amber-500/30 rounded-xl text-amber-300 text-xs flex items-center gap-2"
        >
          <AlertTriangle className="h-4 w-4 text-amber-400 shrink-0" />
          <span>{data.error}</span>
        </div>
      )}

      <TopologyStats
        stacks={stacks}
        releases={authz.isAdmin ? releases : tenantReleases}
      />

      {managementStack && (
        <ManagementPlaneStrip
          stack={managementStack}
          onNavigateToProviders={
            authz.isAdmin ? onNavigateToProviders : undefined
          }
        />
      )}

      <CanaryTrafficSection canaries={canaries} />

      <TopologySection
        testId="topology-clusters-section"
        icon={<Activity className="h-5 w-5 text-indigo-400" />}
        title="Provisioned Stacks & Workload Clusters"
        subtitle="Management & workload Kubernetes clusters managed by PKO v2"
        action={deployButton("cluster", "+ Deploy Cluster")}
        footer={
          stackBanner && (
            <PrNotificationBanner
              message={stackBanner.message}
              prId={stackBanner.prId}
              prScope={stackBanner.prScope}
              prUrl={stackBanner.prUrl}
              onNavigateToPreview={onNavigateToPreview}
              onDismiss={() => setStackBanner(null)}
              className="rounded-none rounded-b-2xl border-x-0 border-b-0 px-6"
            />
          )
        }
      >
        <DataTable
          data={byKind.cluster}
          columns={stackColumns.cluster}
          filterPlaceholder="Filter clusters by name, tenant, provider, status..."
          emptyMessage="No Kubernetes clusters provisioned yet — create a proposal from the Catalog."
          noResultsMessage="No clusters matching your filter."
          getRowId={rowId}
        />
      </TopologySection>

      <TopologySection
        testId="topology-vms-section"
        icon={<Activity className="h-5 w-5 text-indigo-400" />}
        title="Virtual Machines"
        subtitle="Cloud & hypervisor virtual machines with interactive SSH access"
        action={deployButton("vm", "+ Deploy VM")}
      >
        <DataTable
          data={byKind.vm}
          columns={stackColumns.vm}
          filterPlaceholder="Filter stacks by name, tenant, provider, status..."
          emptyMessage="No virtual machines provisioned yet."
          noResultsMessage="No virtual machines matching your filter."
          getRowId={rowId}
        />
      </TopologySection>

      <TopologySection
        testId="topology-services-section"
        icon={<Activity className="h-5 w-5 text-indigo-400" />}
        title="Managed Services & Config"
        subtitle="Cloud databases, network infrastructure, and cluster configuration stacks"
        action={deployButton("service", "+ Deploy Service")}
      >
        <DataTable
          data={byKind.service}
          columns={stackColumns.service}
          filterPlaceholder="Filter services by name, tenant, provider, status..."
          emptyMessage="No managed services provisioned yet."
          noResultsMessage="No managed services matching your filter."
          getRowId={rowId}
        />
      </TopologySection>

      <TopologySection
        testId="topology-releases-section"
        icon={<Package className="h-5 w-5 text-emerald-400" />}
        title="Deployed Application Releases"
        subtitle={
          hasManagementAddons
            ? "Protected management-plane add-ons and Flux HelmReleases & Kustomizations reconciled across tenant namespaces"
            : "Flux HelmReleases & Kustomizations reconciled across tenant namespaces"
        }
        action={deployButton("app", "+ Deploy App")}
        footer={
          releaseBanner && (
            <PrNotificationBanner
              message={releaseBanner.message}
              prId={releaseBanner.prId}
              prScope={releaseBanner.prScope}
              prUrl={releaseBanner.prUrl}
              onNavigateToPreview={onNavigateToPreview}
              onDismiss={() => setReleaseBanner(null)}
              className="rounded-none rounded-b-2xl border-x-0 border-b-0 px-6"
            />
          )
        }
      >
        <ReleaseRowBusyContext.Provider value={releaseBusy}>
          <DataTable
            data={tableReleases}
            columns={releaseColumns}
            filterPlaceholder={
              hasManagementAddons
                ? "Filter releases by name, namespace, chart, status, management..."
                : "Filter releases by name, namespace, chart, status..."
            }
            emptyMessage="No application releases deployed yet — deploy a Helm chart from the Catalog."
            noResultsMessage="No application releases matching your filter."
            getRowId={releaseRowId}
            getRowProps={releaseRowProps}
          />
        </ReleaseRowBusyContext.Provider>
      </TopologySection>

      {/* Always mounted for admins, independent of the table rows. */}
      {authz.isAdmin && (authenticated || demo) && (
        <ManagementAddonDialogs actions={addonActions} />
      )}

      <TelemetryStreamPanel
        filteredEvents={filteredEvents}
        eventFilterKind={eventFilterKind}
        onEventFilterKindChange={setEventFilterKind}
        isStreamPaused={data.paused}
        onToggleStreamPaused={() => data.setPaused(!data.paused)}
        onClearEvents={data.clearEvents}
        onInspectStack={(namespace, name) => {
          const stack = stacks.find(
            (s) => s.name === name && stackNamespace(s) === namespace
          )
          setInspected({
            namespace,
            name,
            provider: stack?.provider,
            resourceKind: stack?.resourceKind,
          })
        }}
      />

      {canaries.length === 0 && (
        <div className="bg-slate-900/60 border border-slate-800/80 rounded-xl px-4 py-3 flex flex-wrap items-center justify-between gap-2 text-xs">
          <span className="flex items-center gap-2 text-slate-400">
            <Gauge className="h-4 w-4 text-amber-400/70" />
            <span className="font-medium text-slate-300">
              Flagger Canary Traffic Rollouts (Envoy Gateway)
            </span>
          </span>
          <span className="text-slate-500">
            No active canary traffic rollouts found.
          </span>
        </div>
      )}

      {inspected && (
        <StackInspectorModal
          namespace={inspected.namespace}
          name={inspected.name}
          kind={inspected.kind}
          tenant={inspected.tenant}
          provider={inspected.provider}
          resourceKind={inspected.resourceKind}
          onClose={() => setInspected(null)}
          onRestarted={() => void data.refresh()}
          onForceDeleted={() => void data.refresh()}
          onOpenSsh={() =>
            setSshStack({
              stackName: inspected.name,
              namespace: inspected.namespace,
              tenant: inspected.tenant || "default",
              provider: inspected.provider,
            })
          }
        />
      )}

      {sshStack && (
        <React.Suspense fallback={null}>
          <VmSshModal
            stackName={sshStack.stackName}
            namespace={sshStack.namespace}
            tenant={sshStack.tenant}
            provider={sshStack.provider}
            onClose={() => setSshStack(null)}
          />
        </React.Suspense>
      )}
    </div>
  )
}
