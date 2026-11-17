import React, { useCallback, useEffect, useMemo, useState } from "react"
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
  GitBranch,
  Package,
  Pencil,
  Trash2,
  Radio,
  Pause,
  Play,
  Filter,
  Gauge,
  Clock,
} from "lucide-react"
import { CrdWatchEvent, ReleaseSummary, StackSummary } from "../types/api"
import {
  createEventSourceStream,
  deleteRelease,
  deleteStack,
  getReleases,
  getStacksWithMeta,
  reconcileRelease,
  restartStackWorkspace,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"
import { StackInspectorModal } from "./StackInspectorModal"
import { StatusBadge } from "./StatusBadge"
import { BundleBadge } from "./BundleBadge"
import { PrNotificationBanner } from "./PrNotificationBanner"
import { DataTable, DataTableColumnDef } from "./DataTable"

const DEFAULT_EVENTS: CrdWatchEvent[] = [
  {
    kind: "GitRepository",
    namespace: "flux-system",
    name: "flux-system",
    status: "Ready",
    message: "Fetched revision main@sha1:a1b2c3d4",
    revision: "main@sha1:a1b2c3d4",
    timestamp: new Date(Date.now() - 30000).toLocaleTimeString(),
  },
  {
    kind: "Kustomization",
    namespace: "tenant-acme",
    name: "tenant-acme-apps",
    status: "Ready",
    message: "Applied revision main@sha1:a1b2c3d4",
    revision: "main@sha1:a1b2c3d4",
    timestamp: new Date(Date.now() - 24000).toLocaleTimeString(),
  },
  {
    kind: "Stack",
    namespace: "keel-system",
    name: "pve-workload-cluster-prod",
    status: "Ready",
    message: "PKO v2 stack update succeeded cleanly (18 resources)",
    revision: "v2.1.0",
    timestamp: new Date(Date.now() - 18000).toLocaleTimeString(),
  },
  {
    kind: "Canary",
    namespace: "tenant-acme",
    name: "frontend-portal-canary",
    status: "Progressing",
    message:
      "Advance frontend-portal-canary weight 40% (5xx rate 0.02%, latency 18ms)",
    revision: "v1.4.0",
    timestamp: new Date(Date.now() - 5000).toLocaleTimeString(),
  },
]

const BOOTSTRAP_MANAGEMENT_STACK: StackSummary = {
  id: "stack-mgmt-01",
  name: "management-cluster-core",
  tenant: "keel-system",
  provider: "kind",
  type: "management",
  status: "synced",
  resourceCount: 24,
  lastSynced: "2 minutes ago",
}

const DEFAULT_STACKS: StackSummary[] = [
  BOOTSTRAP_MANAGEMENT_STACK,
  {
    id: "stack-pve-01",
    name: "pve-workload-cluster-prod",
    tenant: "tenant-acme",
    provider: "proxmox",
    type: "workload",
    status: "synced",
    resourceCount: 18,
    lastSynced: "5 minutes ago",
  },
  {
    id: "stack-aws-01",
    name: "aws-us-east-1-cluster",
    tenant: "tenant-globex",
    provider: "aws",
    type: "cloud",
    status: "reconciling",
    resourceCount: 42,
    lastSynced: "1 minute ago",
  },
  {
    id: "stack-hetzner-01",
    name: "hetzner-fsn1-nodes",
    tenant: "tenant-stark",
    provider: "hetzner",
    type: "workload",
    status: "drift",
    resourceCount: 12,
    lastSynced: "12 minutes ago",
  },
]

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
  } | null>(null)
  const [restartingStackKey, setRestartingStackKey] = useState<string | null>(
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

  const handleQuickRestart = async (stack: StackSummary) => {
    const ns = resolveStackNamespace(stack)
    const key = `${ns}/${stack.name}`
    setRestartingStackKey(key)
    setStackBanner(null)
    try {
      const res = await restartStackWorkspace(ns, stack.name)
      setStackBanner({ message: res.message })
      window.setTimeout(fetchTopology, 1500)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to restart stack workspace"
      )
    } finally {
      setRestartingStackKey(null)
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
      window.setTimeout(fetchTopology, 1500)
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
      window.setTimeout(fetchTopology, 1000)
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
      window.setTimeout(fetchTopology, 1000)
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

  const stackColumns = useMemo<DataTableColumnDef<StackSummary>[]>(
    () => [
      {
        accessorKey: "name",
        header: "Stack Name",
        cell: ({ row }) => (
          <span className="font-semibold text-slate-100">
            {row.original.name}
          </span>
        ),
      },
      {
        accessorKey: "tenant",
        header: "Tenant",
        cell: ({ row }) => (
          <span className="text-indigo-400">{row.original.tenant}</span>
        ),
      },
      {
        accessorKey: "provider",
        header: "Provider",
        cell: ({ row }) => (
          <span className="uppercase text-slate-400">
            {row.original.provider}
          </span>
        ),
      },
      {
        accessorKey: "type",
        header: "Type",
        cell: ({ row }) => (
          <span className="capitalize text-slate-400">
            {row.original.type || "workload"}
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
        accessorKey: "resourceCount",
        header: "Resources",
        cell: ({ row }) => (
          <span className="font-bold text-slate-200">
            {row.original.resourceCount ?? "—"}
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
              <button
                type="button"
                title="Inspect"
                aria-label={`Inspect stack ${stack.name}`}
                onClick={() =>
                  setInspectedStack({ namespace: ns, name: stack.name })
                }
                className="p-1.5 rounded-md bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
              >
                <Terminal className="h-3.5 w-3.5" />
              </button>
              <button
                type="button"
                title="Restart Workspace"
                aria-label={`Restart stack ${stack.name}`}
                disabled={restartingStackKey === `${ns}/${stack.name}`}
                onClick={() => handleQuickRestart(stack)}
                className="p-1.5 rounded-md bg-amber-500/10 hover:bg-amber-500/20 text-amber-300 border border-amber-500/30 transition-colors cursor-pointer disabled:opacity-50"
              >
                <RotateCw
                  className={`h-3.5 w-3.5 ${
                    restartingStackKey === `${ns}/${stack.name}`
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
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [onEditStack, restartingStackKey, deletingKey]
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
                className="p-1.5 rounded-md bg-amber-500/10 hover:bg-amber-500/20 text-amber-300 border border-amber-500/30 transition-colors cursor-pointer disabled:opacity-50"
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
      {canaries.length > 0 && (
        <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
          <div className="flex items-center justify-between border-b border-slate-800 pb-3">
            <div className="flex items-center space-x-3">
              <div className="p-2.5 bg-amber-500/10 text-amber-400 rounded-xl border border-amber-500/20">
                <Gauge className="h-5 w-5" />
              </div>
              <div>
                <h3 className="text-base font-semibold text-white">
                  Flagger Canary Traffic Rollouts (Envoy Gateway)
                </h3>
                <p className="text-xs text-slate-400">
                  Automated progressive traffic shifting with Prometheus error
                  rate &amp; p99 latency evaluation
                </p>
              </div>
            </div>
          </div>

          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
            {canaries.map((canary) => {
              const weight = canary.canaryWeight ?? 0
              const phase = canary.canaryPhase ?? ""
              const isPromoted =
                weight === 0 &&
                (phase === "Succeeded" ||
                  phase === "Initialized" ||
                  phase === "Promoting" ||
                  phase === "Finalising")
              const primaryPct = isPromoted ? 100 : Math.max(0, 100 - weight)
              const primaryVer =
                canary.primaryVersion ||
                (isPromoted ? canary.version : undefined) ||
                "—"
              const canaryVer = canary.version || "—"
              const errRate = canary.errorRate
              const latency = canary.latencyMs

              return (
                <div
                  key={canary.id || `${canary.namespace}-${canary.name}`}
                  className="bg-slate-950 border border-slate-800 rounded-xl p-4 space-y-3"
                >
                  <div className="flex items-center justify-between">
                    <span className="text-xs font-bold text-white flex items-center gap-1.5">
                      <GitBranch className="h-3.5 w-3.5 text-amber-400" />
                      {canary.name}
                    </span>
                    <StatusBadge status={phase || canary.status} size="xs" />
                  </div>

                  <div className="space-y-1.5">
                    <div className="flex items-center justify-between text-[11px]">
                      <span className="text-slate-400">
                        {isPromoted
                          ? "Traffic Split (Promoted)"
                          : "Canary Traffic Weight"}
                      </span>
                      <span
                        className={`font-mono font-bold ${
                          isPromoted
                            ? "text-emerald-400"
                            : phase === "Failed"
                              ? "text-red-400"
                              : "text-amber-400"
                        }`}
                      >
                        {isPromoted ? "100% Primary" : `${weight}%`}
                      </span>
                    </div>
                    <div className="w-full bg-slate-900 rounded-full h-2 overflow-hidden border border-slate-800 flex">
                      <div
                        className={`h-2 transition-all duration-500 ${
                          isPromoted
                            ? "bg-linear-to-r from-emerald-500 to-emerald-400"
                            : phase === "Failed"
                              ? "bg-red-500/80"
                              : "bg-indigo-500"
                        }`}
                        style={{ width: `${primaryPct}%` }}
                        title={`Primary ${primaryVer}: ${primaryPct}%`}
                      ></div>
                      {!isPromoted && weight > 0 && (
                        <div
                          className="bg-linear-to-r from-amber-400 to-emerald-400 h-2 transition-all duration-500"
                          style={{ width: `${weight}%` }}
                          title={`Canary ${canaryVer}: ${weight}%`}
                        ></div>
                      )}
                    </div>
                    <div className="flex items-center justify-between text-[10px] font-mono text-slate-400">
                      <span>
                        Primary:{" "}
                        <strong className="text-indigo-300">
                          {primaryVer}
                        </strong>{" "}
                        ({primaryPct}%)
                      </span>
                      {!isPromoted && (
                        <span>
                          Canary:{" "}
                          <strong className="text-amber-300">
                            {canaryVer}
                          </strong>{" "}
                          ({weight}%)
                        </span>
                      )}
                    </div>
                  </div>

                  <div className="grid grid-cols-2 gap-2 pt-1 text-[10px] font-mono">
                    <div className="bg-slate-900 p-2 rounded border border-slate-800">
                      <span className="text-slate-500 block">
                        HTTP 5xx Rate:
                      </span>
                      <span
                        className={`font-bold ${
                          errRate == null
                            ? "text-slate-500"
                            : errRate > 1
                              ? "text-red-400"
                              : "text-emerald-400"
                        }`}
                      >
                        {errRate != null ? `${errRate}%` : "—"}
                      </span>
                    </div>
                    <div className="bg-slate-900 p-2 rounded border border-slate-800">
                      <span className="text-slate-500 block">p99 Latency:</span>
                      <span
                        className={`font-bold ${
                          latency == null
                            ? "text-slate-500"
                            : latency > 500
                              ? "text-amber-400"
                              : "text-indigo-300"
                        }`}
                      >
                        {latency != null ? `${latency}ms` : "—"}
                      </span>
                    </div>
                  </div>
                </div>
              )
            })}
          </div>
        </div>
      )}

      {/* Provisioned Stacks Table */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Activity className="h-5 w-5 text-indigo-400" />
              Provisioned Stacks &amp; Workload Clusters
            </h3>
            <p className="text-xs text-slate-400">
              Live Pulumi/OpenTofu stacks managed by PKO v2
            </p>
          </div>

          {onNavigateToCatalog && (
            <button
              type="button"
              onClick={onNavigateToCatalog}
              className="px-3 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer self-start sm:self-auto"
            >
              + Deploy Stack
            </button>
          )}
        </div>

        <DataTable
          data={stacks}
          columns={stackColumns}
          filterPlaceholder="Filter stacks by name, tenant, provider, status..."
          emptyMessage="No stacks provisioned yet — authenticate and create a proposal from the Catalog."
          noResultsMessage="No stacks matching your filter."
          getRowId={(row) => row.id || `${row.namespace}-${row.name}`}
        />
        {stackBanner && (
          <PrNotificationBanner
            message={stackBanner.message}
            prId={stackBanner.prId}
            prUrl={stackBanner.prUrl}
            onNavigateToPreview={onNavigateToPreview}
            onDismiss={() => setStackBanner(null)}
            className="rounded-none rounded-b-2xl border-x-0 border-b-0 px-6"
          />
        )}
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
      <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 border-b border-slate-800 pb-4">
          <div className="flex items-center space-x-3">
            <Radio className="h-5 w-5 text-indigo-400" />
            <div>
              <h3 className="text-base font-semibold text-white">
                Live Reconciler Event Stream
              </h3>
              <p className="text-xs text-slate-400">
                kube-rs CRD condition transitions for Flux &amp; PKO v2 Stacks
              </p>
            </div>
          </div>

          <div className="flex items-center space-x-3">
            <div className="flex items-center space-x-1 bg-slate-950 border border-slate-800 rounded-lg p-1">
              <Filter className="h-3.5 w-3.5 text-slate-500 ml-2" />
              <select
                aria-label="Filter events by CRD kind"
                value={eventFilterKind}
                onChange={(e) => setEventFilterKind(e.target.value)}
                style={{ colorScheme: "dark" }}
                className="bg-slate-950 border-0 text-xs text-slate-200 rounded focus:outline-none focus:ring-0 py-0.5 px-2"
              >
                <option value="all" className="bg-slate-950 text-slate-200">
                  All CRDs
                </option>
                <option
                  value="GitRepository"
                  className="bg-slate-950 text-slate-200"
                >
                  GitRepository
                </option>
                <option
                  value="Kustomization"
                  className="bg-slate-950 text-slate-200"
                >
                  Kustomization
                </option>
                <option
                  value="HelmRelease"
                  className="bg-slate-950 text-slate-200"
                >
                  HelmRelease
                </option>
                <option value="Stack" className="bg-slate-950 text-slate-200">
                  Stack
                </option>
                <option value="Canary" className="bg-slate-950 text-slate-200">
                  Canary
                </option>
              </select>
            </div>

            <button
              type="button"
              onClick={() => setIsStreamPaused(!isStreamPaused)}
              className={`flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors cursor-pointer ${
                isStreamPaused
                  ? "bg-amber-500/10 text-amber-400 border-amber-500/30 hover:bg-amber-500/20"
                  : "bg-slate-800 text-slate-300 border-slate-700 hover:bg-slate-700"
              }`}
            >
              {isStreamPaused ? (
                <>
                  <Play className="h-3.5 w-3.5" />
                  <span>Resume Stream</span>
                </>
              ) : (
                <>
                  <Pause className="h-3.5 w-3.5" />
                  <span>Pause Stream</span>
                </>
              )}
            </button>

            <button
              type="button"
              onClick={() => setEvents([])}
              className="p-1.5 text-slate-400 hover:text-red-400 bg-slate-950 hover:bg-slate-800 border border-slate-800 rounded-lg transition-colors cursor-pointer"
              title="Clear event logs"
            >
              <Trash2 className="h-4 w-4" />
            </button>
          </div>
        </div>

        <div className="bg-slate-950 border border-slate-800 rounded-xl p-4 font-mono text-xs space-y-2 max-h-96 overflow-y-auto">
          {filteredEvents.map((evt, idx) => (
            <div
              key={`${evt.name}-${idx}`}
              className="flex flex-col sm:flex-row sm:items-center justify-between p-2.5 rounded-lg bg-slate-900/60 border border-slate-800/80 hover:bg-slate-900 transition-colors gap-2"
            >
              <div className="flex items-start sm:items-center space-x-3 min-w-0 flex-1">
                <span className="text-[10px] text-slate-500 flex items-center gap-1 shrink-0 mt-0.5 sm:mt-0">
                  <Clock className="h-3 w-3" />
                  {evt.timestamp}
                </span>

                <span
                  className={`text-[10px] px-2 py-0.5 rounded font-bold uppercase shrink-0 ${
                    evt.kind === "GitRepository"
                      ? "bg-purple-500/10 text-purple-400 border border-purple-500/20"
                      : evt.kind === "Kustomization"
                        ? "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                        : evt.kind === "HelmRelease"
                          ? "bg-indigo-500/10 text-indigo-400 border border-indigo-500/20"
                          : evt.kind === "Stack"
                            ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                            : "bg-amber-500/10 text-amber-400 border border-amber-500/20"
                  }`}
                >
                  {evt.kind}
                </span>

                <div className="min-w-0 flex-1">
                  <span className="font-semibold text-slate-200">
                    {evt.namespace}/{evt.name}
                  </span>
                  <p className="text-slate-400 truncate mt-0.5">
                    {evt.message}
                  </p>
                </div>
              </div>

              <div className="flex items-center space-x-2 shrink-0 self-end sm:self-center">
                {evt.kind === "Stack" && (
                  <button
                    type="button"
                    onClick={() =>
                      setInspectedStack({
                        namespace: evt.namespace,
                        name: evt.name,
                      })
                    }
                    className="inline-flex items-center space-x-1 text-[10px] px-2 py-0.5 rounded bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                  >
                    <Terminal className="h-3 w-3" />
                    <span>Inspect</span>
                  </button>
                )}
                {evt.revision && (
                  <span
                    title={evt.revision}
                    className="text-[10px] px-1.5 py-0.5 rounded bg-slate-950 border border-slate-800 text-slate-400 max-w-44 truncate"
                  >
                    {evt.revision}
                  </span>
                )}
                <span
                  className={`text-[10px] px-2 py-0.5 rounded font-bold ${
                    evt.status === "Ready" || evt.status === "Healthy"
                      ? "text-emerald-400 bg-emerald-500/10 border border-emerald-500/20"
                      : evt.status === "Progressing"
                        ? "text-blue-400 bg-blue-500/10 border border-blue-500/20"
                        : "text-red-400 bg-red-500/10 border border-red-500/20"
                  }`}
                >
                  {evt.status}
                </span>
              </div>
            </div>
          ))}

          {filteredEvents.length === 0 && (
            <div className="py-8 text-center text-slate-500 font-sans">
              No CRD events received yet or stream cleared.
            </div>
          )}
        </div>
      </div>

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
          onClose={() => setInspectedStack(null)}
          onRestarted={fetchTopology}
        />
      )}
    </div>
  )
}
