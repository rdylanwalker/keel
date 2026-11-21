import React, { useCallback, useEffect, useMemo, useRef, useState } from "react"
import {
  Server,
  Layers,
  CheckCircle2,
  AlertTriangle,
  RefreshCw,
  Activity,
  Boxes,
  RotateCw,
  Terminal,
  Package,
  Pencil,
  Trash2,
  Gauge,
} from "lucide-react"
import { CrdWatchEvent, ReleaseSummary, StackSummary } from "../types/api"
import {
  createEventSourceStream,
  deleteRelease,
  deleteStack,
  getReleases,
  getStacksWithMeta,
  reconcileRelease,
  reconcileStack,
} from "../lib/api"
import {
  isDemoMode,
  isStackSshSupported,
  classifyStackResourceKind,
} from "../lib/utils"
import {
  BOOTSTRAP_MANAGEMENT_STACK,
  DEFAULT_EVENTS,
  DEFAULT_STACKS,
} from "../lib/demoFixtures"
import { StackInspectorModal } from "./StackInspectorModal"
import { VmSshModal } from "./VmSshModal"
import { StatusBadge } from "./StatusBadge"
import { BundleBadge } from "./BundleBadge"
import { PrNotificationBanner } from "./PrNotificationBanner"
import { DataTable, DataTableColumnDef } from "./DataTable"
import { CanaryTrafficSection } from "./CanaryTrafficSection"
import { TelemetryStreamPanel } from "./TelemetryStreamPanel"

function ensureManagementStack(stackList: StackSummary[]): StackSummary[] {
  if (
    stackList.some(
      (s) => s.type === "management" || s.name === "management-cluster-core"
    )
  ) {
    return stackList
  }
  return [BOOTSTRAP_MANAGEMENT_STACK, ...stackList]
}

function formatLastSynced(value?: string): string {
  if (!value || !value.trim()) {
    return "—"
  }
  const parsed = Date.parse(value)
  if (Number.isNaN(parsed)) {
    return value
  }
  const diffSec = Math.max(0, Math.floor((Date.now() - parsed) / 1000))
  if (diffSec < 60) {
    return "just now"
  }
  const diffMin = Math.floor(diffSec / 60)
  if (diffMin < 60) {
    return `${diffMin}m ago`
  }
  const diffHr = Math.floor(diffMin / 60)
  if (diffHr < 24) {
    return `${diffHr}h ago`
  }
  const diffDays = Math.floor(diffHr / 24)
  return `${diffDays}d ago`
}

export interface TopologyViewProps {
  onNavigateToCatalog?: () => void
  onNavigateToPreview?: () => void
  onNavigateToProviders?: () => void
  onEditRelease?: (namespace: string, name: string) => void
  onEditStack?: (namespace: string, name: string) => void
  authenticated?: boolean
  selectedTenant?: string
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
  const [stacks, setStacks] = useState<StackSummary[]>(() =>
    isDemoMode() ? DEFAULT_STACKS : []
  )
  const [releases, setReleases] = useState<ReleaseSummary[]>([])
  const [loading, setLoading] = useState(false)
  const [autoRefresh, setAutoRefresh] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [inspectedStack, setInspectedStack] = useState<{
    namespace: string
    name: string
    kind?: "stack" | "release"
    tenant?: string
    provider?: string
  } | null>(null)
  const [sshStack, setSshStack] = useState<{
    stackName: string
    namespace: string
    tenant: string
    provider?: string
  } | null>(null)
  const [reconcilingStackKey, setReconcilingStackKey] = useState<string | null>(
    null
  )
  const [reconcilingReleaseKey, setReconcilingReleaseKey] = useState<
    string | null
  >(null)
  const [events, setEvents] = useState<CrdWatchEvent[]>(() =>
    isDemoMode() ? DEFAULT_EVENTS : []
  )
  const [isStreamPaused, setIsStreamPaused] = useState(false)
  const [eventFilterKind, setEventFilterKind] = useState<string>("all")
  const [deletingKey, setDeletingKey] = useState<string | null>(null)
  const [stackBanner, setStackBanner] = useState<{
    message: string
    prUrl?: string
    prId?: string
  } | null>(null)
  const [releaseBanner, setReleaseBanner] = useState<{
    message: string
    prUrl?: string
    prId?: string
  } | null>(null)

  const pendingTimeoutsRef = useRef<Set<number>>(new Set())

  useEffect(() => {
    const timeouts = pendingTimeoutsRef.current
    return () => {
      timeouts.forEach((id) => window.clearTimeout(id))
      timeouts.clear()
    }
  }, [])

  const resolveStackNamespace = (stack: StackSummary) =>
    stack.namespace ||
    (stack.name === "management-cluster-core"
      ? "flux-system"
      : stack.tenant || "default")

  const fetchTopology = useCallback(async () => {
    setLoading(true)
    try {
      const [stacksRes, releasesData] = await Promise.all([
        getStacksWithMeta(selectedTenant),
        getReleases(selectedTenant).catch(() => []),
      ])
      setError(null)
      setStacks(ensureManagementStack(stacksRes.stacks || []))
      setReleases(releasesData || [])
    } catch (err) {
      const msg =
        err instanceof Error
          ? err.message
          : "Failed to load live stack topology"
      if (!(isDemoMode() && msg.includes("API error (401)"))) {
        setError(msg)
      }
      if (!isDemoMode()) {
        setStacks(ensureManagementStack([]))
      }
    } finally {
      setLoading(false)
    }
  }, [selectedTenant])

  const scheduleTopologyRefresh = useCallback(
    (delayMs: number) => {
      const id = window.setTimeout(() => {
        pendingTimeoutsRef.current.delete(id)
        void fetchTopology()
      }, delayMs)
      pendingTimeoutsRef.current.add(id)
    },
    [fetchTopology]
  )

  const handleQuickStackReconcile = async (stack: StackSummary) => {
    const ns = resolveStackNamespace(stack)
    const key = `${ns}/${stack.name}`
    setReconcilingStackKey(key)
    setStackBanner(null)
    try {
      const res = await reconcileStack(ns, stack.name)
      setStackBanner({ message: res.message })
      scheduleTopologyRefresh(1500)
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Failed to reconcile stack")
    } finally {
      setReconcilingStackKey(null)
    }
  }

  const handleQuickReconcile = async (rel: ReleaseSummary) => {
    const ns = rel.namespace || "tenant-default"
    const key = `${ns}/${rel.name}`
    setReconcilingReleaseKey(key)
    setReleaseBanner(null)
    try {
      const res = await reconcileRelease(ns, rel.name)
      setReleaseBanner({ message: res.message })
      scheduleTopologyRefresh(1500)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to reconcile release"
      )
    } finally {
      setReconcilingReleaseKey(null)
    }
  }

  const handleDeleteStack = async (stack: StackSummary) => {
    const ns = resolveStackNamespace(stack)
    const key = `stack:${ns}/${stack.name}`
    setDeletingKey(key)
    setStackBanner(null)
    setError(null)
    try {
      const res = await deleteStack(ns, stack.name)
      setStackBanner({
        message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for stack ${stack.name}`,
        prUrl: res.proposal.prUrl,
        prId: res.proposal.id,
      })
      scheduleTopologyRefresh(1000)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to open delete stack PR"
      )
    } finally {
      setDeletingKey(null)
    }
  }

  const handleDeleteRelease = async (rel: ReleaseSummary) => {
    const ns = rel.namespace || "tenant-default"
    const key = `release:${ns}/${rel.name}`
    setDeletingKey(key)
    setReleaseBanner(null)
    setError(null)
    try {
      const res = await deleteRelease(ns, rel.name)
      setReleaseBanner({
        message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for release ${rel.name}`,
        prUrl: res.proposal.prUrl,
        prId: res.proposal.id,
      })
      scheduleTopologyRefresh(1000)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to open delete release PR"
      )
    } finally {
      setDeletingKey(null)
    }
  }

  useEffect(() => {
    if (!authenticated) {
      return
    }
    let cancelled = false
    Promise.all([
      getStacksWithMeta(selectedTenant),
      getReleases(selectedTenant).catch(() => []),
    ])
      .then(([stacksRes, releasesData]) => {
        if (!cancelled) {
          setError(null)
          setStacks(ensureManagementStack(stacksRes.stacks || []))
          setReleases(releasesData || [])
        }
      })
      .catch((err) => {
        if (!cancelled) {
          const msg =
            err instanceof Error
              ? err.message
              : "Failed to load live stack topology"
          if (!(isDemoMode() && msg.includes("API error (401)"))) {
            setError(msg)
          }
          if (!isDemoMode()) {
            setStacks(ensureManagementStack([]))
          }
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [authenticated, selectedTenant])

  useEffect(() => {
    if (!authenticated || !autoRefresh) {
      return
    }
    const timer = window.setInterval(fetchTopology, 5000)
    return () => window.clearInterval(timer)
  }, [authenticated, autoRefresh, fetchTopology])

  useEffect(() => {
    if (!authenticated || isStreamPaused || isDemoMode()) return

    let eventSource: EventSource | null = null
    try {
      eventSource = createEventSourceStream(
        (newEvent) => {
          setEvents((prev) => {
            const formattedTimestamp =
              newEvent.timestamp &&
              !Number.isNaN(Date.parse(newEvent.timestamp))
                ? new Date(newEvent.timestamp).toLocaleTimeString()
                : newEvent.timestamp || new Date().toLocaleTimeString()
            const normalized: CrdWatchEvent = {
              ...newEvent,
              timestamp: formattedTimestamp,
            }
            const isDuplicate = prev.some(
              (e) =>
                e.kind === normalized.kind &&
                e.namespace === normalized.namespace &&
                e.name === normalized.name &&
                e.status === normalized.status &&
                e.message === normalized.message &&
                (e.revision ?? null) === (normalized.revision ?? null) &&
                e.timestamp === normalized.timestamp
            )
            if (isDuplicate) {
              return prev
            }
            return [normalized, ...prev.slice(0, 99)]
          })
        },
        () => {
          // Stream error handler
        }
      )
    } catch {
      // EventSource failover
    }

    return () => {
      if (eventSource) {
        eventSource.close()
      }
    }
  }, [authenticated, isStreamPaused])

  const normalizeStatus = (status?: string) => (status ?? "").toLowerCase()

  const createStackColumns = (headerName: string, includeSsh: boolean) =>
    [
      {
        accessorKey: "name",
        header: headerName,
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <span className="font-semibold text-slate-100">
            {row.original.name}
          </span>
        ),
      },
      {
        accessorKey: "tenant",
        header: "Tenant",
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <span className="text-indigo-400">{row.original.tenant}</span>
        ),
      },
      {
        accessorKey: "provider",
        header: "Provider",
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <span className="uppercase tracking-wider font-mono text-[10px]">
            {row.original.provider}
          </span>
        ),
      },
      {
        accessorKey: "type",
        header: "Type",
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <span className="text-slate-300 capitalize">{row.original.type}</span>
        ),
      },
      {
        accessorKey: "status",
        header: "Status",
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <StatusBadge status={row.original.status} />
        ),
      },
      {
        accessorKey: "resourceCount",
        header: "Resources",
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <span className="font-mono text-slate-300">
            {row.original.resourceCount ?? "-"}
          </span>
        ),
      },
      {
        accessorKey: "bundle",
        header: "Bundle",
        cell: ({ row }: { row: { original: StackSummary } }) => {
          if (!row.original.bundle) return null
          return (
            <BundleBadge
              bundle={row.original.bundle}
              bundleItem={row.original.bundleItem}
            />
          )
        },
      },
      {
        accessorKey: "lastSynced",
        header: "Last Synced",
        cell: ({ row }: { row: { original: StackSummary } }) => (
          <span
            className="text-slate-400 whitespace-nowrap"
            title={row.original.lastSynced || undefined}
          >
            {formatLastSynced(row.original.lastSynced)}
          </span>
        ),
      },
      {
        id: "actions",
        header: "",
        cell: ({ row }: { row: { original: StackSummary } }) => {
          const stack = row.original
          const ns =
            stack.namespace ||
            (stack.name === "management-cluster-core"
              ? "flux-system"
              : stack.tenant?.startsWith("tenant-")
                ? stack.tenant
                : stack.tenant || "default")
          const isProtected =
            stack.name === "management-cluster-core" ||
            stack.namespace === "flux-system"
          const sshSupported = includeSsh ? isStackSshSupported(stack) : false
          return (
            <div className="inline-flex items-center justify-end gap-1.5">
              {onEditStack && (
                <button
                  type="button"
                  title={isProtected ? "Protected control-plane stack" : "Edit"}
                  aria-label={`Edit stack ${stack.name}`}
                  disabled={isProtected}
                  onClick={() => onEditStack(ns, stack.name)}
                  className="p-1.5 rounded-md bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                >
                  <Pencil className="h-3.5 w-3.5" />
                </button>
              )}
              {includeSsh && (
                <button
                  type="button"
                  data-testid={`stack-ssh-btn-${stack.name}`}
                  disabled={!sshSupported}
                  title={
                    sshSupported
                      ? "SSH Connect"
                      : "SSH is not supported for this infrastructure"
                  }
                  aria-label={`SSH connect to stack ${stack.name}`}
                  onClick={() =>
                    setSshStack({
                      stackName: stack.name,
                      namespace: ns,
                      tenant: stack.tenant || "default",
                      provider: stack.provider,
                    })
                  }
                  className="p-1.5 rounded-md bg-emerald-500/10 hover:bg-emerald-500/20 text-emerald-300 border border-emerald-500/30 transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                >
                  <Terminal className="h-3.5 w-3.5" />
                </button>
              )}
              <button
                type="button"
                title="Inspect"
                aria-label={`Inspect stack ${stack.name}`}
                onClick={() =>
                  setInspectedStack({
                    namespace: ns,
                    name: stack.name,
                    tenant: stack.tenant,
                    provider: stack.provider,
                  })
                }
                className="p-1.5 rounded-md bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
              >
                <Terminal className="h-3.5 w-3.5" />
              </button>
              <button
                type="button"
                title="Reconcile Now"
                aria-label={`Reconcile stack ${stack.name}`}
                disabled={reconcilingStackKey === `${ns}/${stack.name}`}
                onClick={() => handleQuickStackReconcile(stack)}
                className="p-1.5 rounded-md bg-blue-500/10 hover:bg-blue-500/20 text-blue-300 border border-blue-500/30 transition-colors cursor-pointer disabled:opacity-50"
              >
                <RotateCw
                  className={`h-3.5 w-3.5 ${
                    reconcilingStackKey === `${ns}/${stack.name}`
                      ? "animate-spin"
                      : ""
                  }`}
                />
              </button>
              <button
                type="button"
                title={
                  isProtected
                    ? "Protected control-plane stack"
                    : "Delete (Open PR)"
                }
                aria-label={`Delete stack ${stack.name}`}
                disabled={
                  isProtected || deletingKey === `stack:${ns}/${stack.name}`
                }
                onClick={() => handleDeleteStack(stack)}
                className="p-1.5 rounded-md bg-red-500/10 hover:bg-red-500/20 text-red-300 border border-red-500/30 transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          )
        },
      },
    ] as DataTableColumnDef<StackSummary>[]

  const stackColumns = useMemo<DataTableColumnDef<StackSummary>[]>(
    () => createStackColumns("Cluster Name", false),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [onEditStack, reconcilingStackKey, deletingKey]
  )
  const vmStackColumns = useMemo<DataTableColumnDef<StackSummary>[]>(
    () => createStackColumns("Stack Name", true),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [onEditStack, reconcilingStackKey, deletingKey]
  )
  const serviceStackColumns = useMemo<DataTableColumnDef<StackSummary>[]>(
    () => createStackColumns("Service Name", false),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [onEditStack, reconcilingStackKey, deletingKey]
  )

  const releaseColumns = useMemo<DataTableColumnDef<ReleaseSummary>[]>(
    () => [
      {
        accessorKey: "name",
        header: "Release Name",
        cell: ({ row }) => {
          const rel = row.original
          return (
            <div className="flex items-center space-x-2 font-semibold text-slate-100">
              <span>{rel.name}</span>
              {(rel.canary || rel.type === "canary") && (
                <span className="text-[10px] px-1.5 py-0.5 rounded bg-purple-500/15 text-purple-300 border border-purple-500/30">
                  {rel.canaryWeight && rel.canaryWeight > 0
                    ? `Canary ${rel.canaryWeight}%`
                    : "Canary"}
                </span>
              )}
              {rel.primaryVersion &&
                rel.version &&
                rel.primaryVersion !== rel.version &&
                rel.canaryPhase !== "Succeeded" && (
                  <span
                    className="text-[10px] font-mono text-amber-300"
                    title="Live primary version → desired canary version"
                  >
                    {rel.primaryVersion} → {rel.version}
                  </span>
                )}
            </div>
          )
        },
      },
      {
        accessorKey: "namespace",
        header: "Namespace",
        cell: ({ row }) => (
          <span className="text-indigo-400">{row.original.namespace}</span>
        ),
      },
      {
        id: "targetCluster",
        accessorFn: (row) =>
          row.targetCluster
            ? row.targetCluster.replace(/-kubeconfig$/, "")
            : "local (mgmt)",
        header: "Target Cluster",
        cell: ({ row }) =>
          row.original.targetCluster ? (
            <span className="inline-flex items-center whitespace-nowrap px-2 py-0.5 rounded bg-cyan-500/10 text-cyan-300 border border-cyan-500/20 text-[11px]">
              {row.original.targetCluster.replace(/-kubeconfig$/, "")}
            </span>
          ) : (
            <span className="text-slate-400 whitespace-nowrap">
              local (mgmt)
            </span>
          ),
      },
      {
        id: "bundle",
        accessorFn: (row) =>
          row.bundle
            ? `${row.bundle}${row.bundleItem ? `/${row.bundleItem}` : ""}`
            : "",
        header: "Bundle",
        cell: ({ row }) => (
          <BundleBadge
            bundle={row.original.bundle}
            bundleItem={row.original.bundleItem}
            fallback={<span className="text-slate-600">—</span>}
          />
        ),
      },
      {
        id: "chart",
        accessorFn: (row) => row.chart || row.name,
        header: "Chart / Source",
        cell: ({ row }) => (
          <span className="text-slate-300">
            {row.original.chart || row.original.name}
          </span>
        ),
      },
      {
        id: "version",
        accessorFn: (row) => row.version || "latest",
        header: "Version",
        cell: ({ row }) => (
          <span className="text-slate-400">
            {row.original.version || "latest"}
          </span>
        ),
      },
      {
        id: "runtime",
        accessorFn: (row) => row.runtime || row.type || "helm",
        header: "Runtime",
        cell: ({ row }) => (
          <span className="uppercase text-slate-400">
            {row.original.runtime || row.original.type || "helm"}
          </span>
        ),
      },
      {
        accessorKey: "status",
        header: "Status",
        cell: ({ row }) => <StatusBadge status={row.original.status} />,
      },
      {
        accessorKey: "lastSynced",
        header: "Last Synced",
        cell: ({ row }) => (
          <span
            className="text-slate-400 whitespace-nowrap"
            title={row.original.lastSynced || undefined}
          >
            {formatLastSynced(row.original.lastSynced)}
          </span>
        ),
      },
      {
        id: "actions",
        header: "Actions",
        enableSorting: false,
        meta: { align: "right" },
        cell: ({ row }) => {
          const rel = row.original
          const ns = rel.namespace || "tenant-default"
          return (
            <div className="inline-flex items-center justify-end gap-1.5">
              {onEditRelease && (
                <button
                  type="button"
                  title="Edit"
                  aria-label={`Edit release ${rel.name}`}
                  onClick={() => onEditRelease(ns, rel.name)}
                  className="p-1.5 rounded-md bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                >
                  <Pencil className="h-3.5 w-3.5" />
                </button>
              )}
              <button
                type="button"
                title="Inspect"
                aria-label={`Inspect release ${rel.name}`}
                onClick={() =>
                  setInspectedStack({
                    namespace: ns,
                    name: rel.name,
                    kind: "release",
                  })
                }
                className="p-1.5 rounded-md bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
              >
                <Terminal className="h-3.5 w-3.5" />
              </button>
              <button
                type="button"
                title="Reconcile Now"
                aria-label={`Reconcile release ${rel.name}`}
                disabled={reconcilingReleaseKey === `${ns}/${rel.name}`}
                onClick={() => handleQuickReconcile(rel)}
                className="p-1.5 rounded-md bg-blue-500/10 hover:bg-blue-500/20 text-blue-300 border border-blue-500/30 transition-colors cursor-pointer disabled:opacity-50"
              >
                <RotateCw
                  className={`h-3.5 w-3.5 ${
                    reconcilingReleaseKey === `${ns}/${rel.name}`
                      ? "animate-spin"
                      : ""
                  }`}
                />
              </button>
              <button
                type="button"
                title="Delete (Open PR)"
                aria-label={`Delete release ${rel.name}`}
                disabled={deletingKey === `release:${ns}/${rel.name}`}
                onClick={() => handleDeleteRelease(rel)}
                className="p-1.5 rounded-md bg-red-500/10 hover:bg-red-500/20 text-red-300 border border-red-500/30 transition-colors cursor-pointer disabled:opacity-40"
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          )
        },
      },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [onEditRelease, reconcilingReleaseKey, deletingKey]
  )

  const allWorkloads = [...stacks, ...releases]

  const syncedCount = allWorkloads.filter((item) => {
    const st = normalizeStatus(item.status)
    return st === "synced" || st === "ready" || st === "healthy"
  }).length
  const reconcilingCount = allWorkloads.filter((item) => {
    const st = normalizeStatus(item.status)
    return st === "reconciling" || st === "progressing"
  }).length
  const driftCount = allWorkloads.filter((item) => {
    const norm = normalizeStatus(item.status)
    return (
      norm === "drift" ||
      norm === "drifted" ||
      norm === "degraded" ||
      norm === "error" ||
      norm === "failed" ||
      norm === "deleting"
    )
  }).length

  const canaries = releases.filter(
    (r) => r.canary || r.type === "canary" || r.canaryWeight !== undefined
  )
  const filteredEvents =
    eventFilterKind === "all"
      ? events
      : events.filter((e) => e.kind === eventFilterKind)

  const clusterStacks = useMemo(
    () => stacks.filter((s) => classifyStackResourceKind(s) === "cluster"),
    [stacks]
  )
  const vmStacks = useMemo(
    () => stacks.filter((s) => classifyStackResourceKind(s) === "vm"),
    [stacks]
  )
  const serviceStacks = useMemo(
    () => stacks.filter((s) => classifyStackResourceKind(s) === "service"),
    [stacks]
  )

  return (
    <div className="space-y-6">
      {/* Overview header stats */}
      <div className="flex flex-col md:flex-row md:items-center justify-between gap-4 bg-slate-900 border border-slate-800 p-6 rounded-2xl">
        <div>
          <h2 className="text-xl font-bold text-white flex items-center gap-2">
            <Boxes className="h-6 w-6 text-indigo-400" />
            Cluster &amp; Workload Topology
          </h2>
          <p className="text-xs text-slate-400 mt-1">
            Monitor provisioned workload stacks, application releases, canary
            rollouts, and live CRD events
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <span
            className={`flex items-center space-x-1.5 px-3 py-1 rounded-full text-xs font-mono border ${
              isStreamPaused
                ? "bg-amber-500/10 text-amber-400 border-amber-500/20"
                : "bg-emerald-500/10 text-emerald-400 border-emerald-500/20"
            }`}
          >
            <span
              className={`w-2 h-2 rounded-full ${
                isStreamPaused ? "bg-amber-400" : "bg-emerald-400"
              }`}
            ></span>
            <span>{isStreamPaused ? "SSE Paused" : "SSE Live Stream"}</span>
          </span>
          <button
            type="button"
            onClick={() => setAutoRefresh((v) => !v)}
            className={`px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors cursor-pointer ${
              autoRefresh
                ? "bg-indigo-600/20 text-indigo-300 border-indigo-500/40"
                : "bg-slate-800 text-slate-400 border-slate-700"
            }`}
          >
            {autoRefresh ? "Auto: 5s" : "Auto: Off"}
          </button>
          <button
            onClick={fetchTopology}
            disabled={loading}
            className="flex items-center space-x-2 px-3 py-1.5 bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 rounded-lg text-xs transition-colors cursor-pointer"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            <span>Refresh Map</span>
          </button>
        </div>
      </div>

      {error && authenticated && (
        <div
          role="alert"
          className="p-4 bg-amber-950/40 border border-amber-500/30 rounded-xl text-amber-300 text-xs flex items-center space-x-2"
        >
          <AlertTriangle className="h-4 w-4 text-amber-400 shrink-0" />
          <span>
            {error}
            {isDemoMode() ? " (showing fallback local topology)" : ""}
          </span>
        </div>
      )}

      {/* Stats row */}
      <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Total Stacks &amp; Apps</p>
            <p className="text-2xl font-bold text-white mt-1">
              {allWorkloads.length}
            </p>
          </div>
          <div className="p-3 bg-indigo-500/10 text-indigo-400 rounded-xl">
            <Layers className="h-6 w-6" />
          </div>
        </div>

        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Synced &amp; Healthy</p>
            <p className="text-2xl font-bold text-emerald-400 mt-1">
              {syncedCount}
            </p>
          </div>
          <div className="p-3 bg-emerald-500/10 text-emerald-400 rounded-xl">
            <CheckCircle2 className="h-6 w-6" />
          </div>
        </div>

        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Reconciling</p>
            <p className="text-2xl font-bold text-blue-400 mt-1">
              {reconcilingCount}
            </p>
          </div>
          <div className="p-3 bg-blue-500/10 text-blue-400 rounded-xl">
            <RefreshCw
              className={`h-6 w-6 ${reconcilingCount > 0 ? "animate-spin" : ""}`}
            />
          </div>
        </div>

        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Drifted / Error / Deleting</p>
            <p className="text-2xl font-bold text-amber-400 mt-1">
              {driftCount}
            </p>
          </div>
          <div className="p-3 bg-amber-500/10 text-amber-400 rounded-xl">
            <AlertTriangle className="h-6 w-6" />
          </div>
        </div>
      </div>

      {/* Compact Management Control Plane Strip */}
      <div className="bg-slate-900 border border-slate-800 rounded-xl px-4 py-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap items-center gap-2.5 text-xs">
            <span className="inline-flex items-center gap-1.5 font-semibold text-white">
              <span className="w-2 h-2 rounded-full bg-emerald-400 animate-pulse" />
              <Server className="h-3.5 w-3.5 text-indigo-400" />
              Management Control Plane
            </span>
            <span className="text-[10px] px-2 py-0.5 rounded-full bg-indigo-500/10 text-indigo-400 border border-indigo-500/30 font-medium">
              protect: true
            </span>
            <span className="text-slate-600">&middot;</span>
            <span className="text-xs font-medium text-emerald-400">
              Healthy
            </span>
          </div>

          {onNavigateToProviders && (
            <button
              type="button"
              onClick={onNavigateToProviders}
              className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 px-2 py-1 cursor-pointer"
            >
              Providers &rarr;
            </button>
          )}
        </div>
      </div>

      {/* Active Flagger Canary Progressive Delivery Section (promoted when active) */}
      <CanaryTrafficSection canaries={canaries} />

      {/* Kubernetes Clusters */}
      <div
        data-testid="topology-clusters-section"
        className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6"
      >
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Activity className="h-5 w-5 text-indigo-400" />
              Provisioned Stacks &amp; Workload Clusters
            </h3>
            <p className="text-xs text-slate-400">
              Management & workload Kubernetes clusters managed by PKO v2
            </p>
          </div>

          {onNavigateToCatalog && (
            <button
              type="button"
              onClick={onNavigateToCatalog}
              className="px-3 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer self-start sm:self-auto"
            >
              + Deploy Cluster
            </button>
          )}
        </div>

        <DataTable
          data={clusterStacks}
          columns={stackColumns}
          filterPlaceholder="Filter clusters by name, tenant, provider, status..."
          emptyMessage="No Kubernetes clusters provisioned yet — authenticate and create a proposal from the Catalog."
          noResultsMessage="No clusters matching your filter."
          getRowId={(row) => row.id || `${row.namespace}-${row.name}`}
        />
        {stackBanner && (
          <PrNotificationBanner
            message={stackBanner.message}
            prId={stackBanner.prId}
            onDismiss={() => setStackBanner(null)}
          />
        )}
      </div>

      {/* Virtual Machines */}
      <div
        data-testid="topology-vms-section"
        className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6"
      >
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Activity className="h-5 w-5 text-indigo-400" />
              Virtual Machines
            </h3>
            <p className="text-xs text-slate-400">
              Cloud & hypervisor virtual machines with interactive SSH access
            </p>
          </div>

          {onNavigateToCatalog && (
            <button
              type="button"
              onClick={onNavigateToCatalog}
              className="px-3 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer self-start sm:self-auto"
            >
              + Deploy VM
            </button>
          )}
        </div>

        <DataTable
          data={vmStacks}
          columns={vmStackColumns}
          filterPlaceholder="Filter stacks by name, tenant, provider, status..."
          emptyMessage="No virtual machines provisioned yet."
          noResultsMessage="No virtual machines matching your filter."
          getRowId={(row) => row.id || `${row.namespace}-${row.name}`}
        />
      </div>

      {/* Managed Services & Config */}
      <div
        data-testid="topology-services-section"
        className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6"
      >
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Activity className="h-5 w-5 text-indigo-400" />
              Managed Services &amp; Config
            </h3>
            <p className="text-xs text-slate-400">
              Cloud databases, network infrastructure, and cluster configuration
              stacks
            </p>
          </div>

          {onNavigateToCatalog && (
            <button
              type="button"
              onClick={onNavigateToCatalog}
              className="px-3 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer self-start sm:self-auto"
            >
              + Deploy Service
            </button>
          )}
        </div>

        <DataTable
          data={serviceStacks}
          columns={serviceStackColumns}
          filterPlaceholder="Filter services by name, tenant, provider, status..."
          emptyMessage="No managed services provisioned yet."
          noResultsMessage="No managed services matching your filter."
          getRowId={(row) => row.id || `${row.namespace}-${row.name}`}
        />
      </div>

      {/* Deployed Application Releases (Flux Helm / Kustomize) */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Package className="h-5 w-5 text-emerald-400" />
              Deployed Application Releases
            </h3>
            <p className="text-xs text-slate-400">
              Flux HelmReleases &amp; Kustomizations reconciled across tenant
              namespaces
            </p>
          </div>
          {onNavigateToCatalog && (
            <button
              type="button"
              onClick={onNavigateToCatalog}
              className="px-3 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer self-start sm:self-auto"
            >
              + Deploy App
            </button>
          )}
        </div>

        <DataTable
          data={releases}
          columns={releaseColumns}
          filterPlaceholder="Filter releases by name, namespace, chart, status..."
          emptyMessage="No application releases deployed yet — deploy a Helm chart from the Catalog."
          noResultsMessage="No application releases matching your filter."
          getRowId={(row) => row.id || `${row.namespace}-${row.name}`}
        />
        {releaseBanner && (
          <PrNotificationBanner
            message={releaseBanner.message}
            prId={releaseBanner.prId}
            prUrl={releaseBanner.prUrl}
            onNavigateToPreview={onNavigateToPreview}
            onDismiss={() => setReleaseBanner(null)}
            className="rounded-none rounded-b-2xl border-x-0 border-b-0 px-6"
          />
        )}
      </div>

      {/* Real-time SSE CRD Event Stream Console */}
      <TelemetryStreamPanel
        filteredEvents={filteredEvents}
        eventFilterKind={eventFilterKind}
        onEventFilterKindChange={setEventFilterKind}
        isStreamPaused={isStreamPaused}
        onToggleStreamPaused={() => setIsStreamPaused(!isStreamPaused)}
        onClearEvents={() => setEvents([])}
        onInspectStack={(namespace, name) =>
          setInspectedStack({
            namespace,
            name,
          })
        }
      />

      {/* Quiet Flagger Canary status when no rollouts are active */}
      {canaries.length === 0 && (
        <div className="bg-slate-900/60 border border-slate-800/80 rounded-xl px-4 py-3 flex items-center justify-between text-xs">
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

      {inspectedStack && (
        <StackInspectorModal
          namespace={inspectedStack.namespace}
          name={inspectedStack.name}
          kind={inspectedStack.kind}
          tenant={inspectedStack.tenant}
          provider={inspectedStack.provider}
          onClose={() => setInspectedStack(null)}
          onRestarted={fetchTopology}
          onForceDeleted={fetchTopology}
          onOpenSsh={() => {
            setSshStack({
              stackName: inspectedStack.name,
              namespace: inspectedStack.namespace,
              tenant: inspectedStack.tenant || "default",
              provider: inspectedStack.provider,
            })
          }}
        />
      )}

      {sshStack && (
        <VmSshModal
          stackName={sshStack.stackName}
          namespace={sshStack.namespace}
          tenant={sshStack.tenant}
          provider={sshStack.provider}
          onClose={() => setSshStack(null)}
        />
      )}
    </div>
  )
}
