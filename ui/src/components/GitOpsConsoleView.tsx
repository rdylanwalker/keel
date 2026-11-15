import React, { useEffect, useState } from "react"
import {
  Activity,
  Radio,
  Pause,
  Play,
  Trash2,
  Filter,
  GitBranch,
  Gauge,
  Clock,
  Terminal,
  Pencil,
} from "lucide-react"
import { CrdWatchEvent, ReleaseSummary } from "../types/api"
import { createEventSourceStream, deleteRelease, getReleases } from "../lib/api"
import { isDemoMode } from "../lib/utils"
import { StackInspectorModal } from "./StackInspectorModal"

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

const DEFAULT_RELEASES: ReleaseSummary[] = [
  {
    id: "rel-canary-01",
    name: "frontend-portal",
    tenant: "tenant-acme",
    namespace: "tenant-acme",
    type: "canary",
    status: "progressing",
    version: "v1.4.0",
    canaryWeight: 40,
    errorRate: 0.02,
    latencyMs: 18,
  },
  {
    id: "rel-helm-01",
    name: "pg-cluster-db",
    tenant: "tenant-acme",
    namespace: "tenant-acme",
    type: "helm",
    status: "healthy",
    version: "15.4.0",
  },
  {
    id: "rel-kust-01",
    name: "envoy-gateway-routes",
    tenant: "keel-system",
    namespace: "keel-system",
    type: "kustomize",
    status: "healthy",
    version: "v1.2.0",
  },
]

export interface GitOpsConsoleViewProps {
  onEditRelease?: (namespace: string, name: string) => void
  onNavigateToPreview?: () => void
}

export const GitOpsConsoleView: React.FC<GitOpsConsoleViewProps> = ({
  onEditRelease,
  onNavigateToPreview,
}) => {
  const [events, setEvents] = useState<CrdWatchEvent[]>(() =>
    isDemoMode() ? DEFAULT_EVENTS : []
  )
  const [releases, setReleases] = useState<ReleaseSummary[]>(() =>
    isDemoMode() ? DEFAULT_RELEASES : []
  )
  const [isPaused, setIsPaused] = useState(false)
  const [filterKind, setFilterKind] = useState<string>("all")
  const [inspectedStack, setInspectedStack] = useState<{
    namespace: string
    name: string
    kind?: "stack" | "release"
  } | null>(null)
  const [deletingKey, setDeletingKey] = useState<string | null>(null)
  const [releaseBanner, setReleaseBanner] = useState<{
    message: string
    prUrl?: string
    prId?: string
  } | null>(null)
  const [deleteError, setDeleteError] = useState<string | null>(null)

  const handleDeleteRelease = async (rel: ReleaseSummary) => {
    const ns = rel.namespace || "tenant-default"
    const key = `${ns}/${rel.name}`
    setDeletingKey(key)
    setReleaseBanner(null)
    setDeleteError(null)
    try {
      const res = await deleteRelease(ns, rel.name)
      setReleaseBanner({
        message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for release ${rel.name}`,
        prUrl: res.proposal.prUrl,
        prId: res.proposal.id,
      })
    } catch (err: unknown) {
      setDeleteError(
        err instanceof Error ? err.message : "Failed to open delete release PR"
      )
    } finally {
      setDeletingKey(null)
    }
  }

  useEffect(() => {
    const loadReleases = async () => {
      try {
        const data = await getReleases()
        if (data && data.length > 0) {
          setReleases(data)
        } else if (!isDemoMode()) {
          setReleases([])
        }
      } catch {
        if (!isDemoMode()) {
          setReleases([])
        }
      }
    }
    loadReleases()
  }, [])

  useEffect(() => {
    if (isPaused) return

    let eventSource: EventSource | null = null
    try {
      eventSource = createEventSourceStream(
        (newEvent) => {
          setEvents((prev) => [
            {
              ...newEvent,
              timestamp: new Date().toLocaleTimeString(),
            },
            ...prev.slice(0, 99), // Keep latest 100 events
          ])
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
  }, [isPaused])

  const handleClear = () => {
    setEvents([])
  }

  const filteredEvents =
    filterKind === "all" ? events : events.filter((e) => e.kind === filterKind)

  const canaries = releases.filter(
    (r) => r.type === "canary" || r.canaryWeight !== undefined
  )

  return (
    <div className="space-y-6">
      {/* Overview Header */}
      <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 bg-slate-900 border border-slate-800 p-6 rounded-2xl">
        <div>
          <h2 className="text-xl font-bold text-white flex items-center gap-2">
            <Activity className="h-6 w-6 text-indigo-400" />
            GitOps &amp; Progressive Delivery Console
          </h2>
          <p className="text-xs text-slate-400 mt-1">
            Monitor live deployment status, progressive canary rollouts, and
            reconciliation events
          </p>
        </div>

        <div className="flex items-center space-x-2">
          <span className="flex items-center space-x-1.5 px-3 py-1 bg-emerald-500/10 text-emerald-400 border border-emerald-500/20 rounded-full text-xs font-mono">
            <span className="w-2 h-2 rounded-full bg-emerald-400 animate-ping"></span>
            <span>SSE Live Stream</span>
          </span>
        </div>
      </div>

      {/* Flagger Canary Progressive Delivery Section */}
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

        {canaries.length === 0 ? (
          <div className="py-6 text-center text-xs text-slate-500 font-sans">
            No active canary traffic rollouts found.
          </div>
        ) : (
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
            {canaries.map((canary) => {
              const weight = canary.canaryWeight ?? 0
              const errRate = canary.errorRate ?? 0.01
              const latency = canary.latencyMs ?? 15

              return (
                <div
                  key={canary.id}
                  className="bg-slate-950 border border-slate-800 rounded-xl p-4 space-y-3"
                >
                  <div className="flex items-center justify-between">
                    <span className="text-xs font-bold text-white flex items-center gap-1.5">
                      <GitBranch className="h-3.5 w-3.5 text-amber-400" />
                      {canary.name}
                    </span>
                    <span
                      className={`text-[10px] font-mono px-2 py-0.5 rounded-full font-medium ${
                        canary.status === "progressing"
                          ? "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                          : "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                      }`}
                    >
                      {canary.status}
                    </span>
                  </div>

                  <div className="space-y-1">
                    <div className="flex items-center justify-between text-[11px]">
                      <span className="text-slate-400">
                        Canary Traffic Weight
                      </span>
                      <span className="font-mono font-bold text-amber-400">
                        {weight}%
                      </span>
                    </div>
                    <div className="w-full bg-slate-900 rounded-full h-2 overflow-hidden border border-slate-800">
                      <div
                        className="bg-gradient-to-r from-indigo-500 to-amber-400 h-2 transition-all duration-500"
                        style={{ width: `${weight}%` }}
                      ></div>
                    </div>
                  </div>

                  <div className="grid grid-cols-2 gap-2 pt-1 text-[10px] font-mono">
                    <div className="bg-slate-900 p-2 rounded border border-slate-800">
                      <span className="text-slate-500 block">
                        HTTP 5xx Rate:
                      </span>
                      <span className="text-emerald-400 font-bold">
                        {errRate}%
                      </span>
                    </div>
                    <div className="bg-slate-900 p-2 rounded border border-slate-800">
                      <span className="text-slate-500 block">p99 Latency:</span>
                      <span className="text-indigo-300 font-bold">
                        {latency}ms
                      </span>
                    </div>
                  </div>
                </div>
              )
            })}
          </div>
        )}
      </div>

      {/* Deployed Releases (Flux Helm / Kustomize) */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="text-base font-semibold text-white">
              Deployed Releases (Flux Helm / Kustomize)
            </h3>
            <p className="text-xs text-slate-400">
              Active application releases reconciled from Git across tenant
              namespaces
            </p>
          </div>
          <span className="text-xs font-mono text-slate-400">
            {releases.length} release{releases.length === 1 ? "" : "s"}
          </span>
        </div>

        {releases.length === 0 ? (
          <div className="p-4 rounded-xl bg-slate-950 border border-slate-800 text-xs text-slate-500 text-center">
            No application releases deployed yet.
          </div>
        ) : (
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
            {releases.map((rel) => {
              const norm = rel.status.toLowerCase()
              const badgeClass =
                norm === "synced" || norm === "ready" || norm === "healthy"
                  ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                  : norm === "reconciling" || norm === "progressing"
                    ? "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                    : norm === "deleting"
                      ? "bg-orange-500/10 text-orange-300 border border-orange-500/30"
                      : norm === "error" ||
                          norm === "failed" ||
                          norm === "drift" ||
                          norm === "degraded"
                        ? "bg-red-500/10 text-red-400 border border-red-500/20"
                        : "bg-slate-500/10 text-slate-300 border border-slate-500/20"
              const runtimeLabel = rel.runtime || rel.type
              return (
                <div
                  key={rel.id || `${rel.namespace}-${rel.name}`}
                  className="bg-slate-950 border border-slate-800 rounded-xl p-4 space-y-2"
                >
                  <div className="flex items-center justify-between gap-2">
                    <div className="flex items-center gap-2 min-w-0">
                      <span className="text-sm font-bold text-white truncate">
                        {rel.name}
                      </span>
                      {rel.bundle && (
                        <span
                          title={
                            rel.bundleItem
                              ? `Bundle: ${rel.bundle} (item: ${rel.bundleItem})`
                              : `Bundle: ${rel.bundle}`
                          }
                          className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-purple-500/15 text-purple-300 border border-purple-500/30 shrink-0"
                        >
                          {rel.bundle}
                          {rel.bundleItem ? `/${rel.bundleItem}` : ""}
                        </span>
                      )}
                    </div>
                    <span
                      className={`text-[10px] px-2 py-0.5 rounded-full font-medium capitalize ${badgeClass}`}
                    >
                      {rel.status}
                    </span>
                  </div>
                  <div className="text-[11px] font-mono text-slate-400 flex items-center justify-between gap-2">
                    <span className="truncate">
                      {rel.namespace} ·{" "}
                      {rel.targetCluster
                        ? rel.targetCluster.replace(/-kubeconfig$/, "")
                        : "local"}
                    </span>
                    <div className="flex items-center gap-2 shrink-0">
                      <span>
                        {runtimeLabel ? `${runtimeLabel} ` : ""}
                        {rel.version || "latest"}
                      </span>
                      {onEditRelease && (
                        <button
                          type="button"
                          title="Edit"
                          aria-label={`Edit release ${rel.name}`}
                          onClick={() =>
                            onEditRelease(
                              rel.namespace || "tenant-default",
                              rel.name
                            )
                          }
                          className="p-1.5 rounded bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                        >
                          <Pencil className="h-3 w-3" />
                        </button>
                      )}
                      <button
                        type="button"
                        title="Inspect"
                        aria-label={`Inspect release ${rel.name}`}
                        onClick={() =>
                          setInspectedStack({
                            namespace: rel.namespace || "tenant-default",
                            name: rel.name,
                            kind: "release",
                          })
                        }
                        className="p-1.5 rounded bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer"
                      >
                        <Terminal className="h-3 w-3 text-indigo-400" />
                      </button>
                      <button
                        type="button"
                        title="Delete (Open PR)"
                        aria-label={`Delete release ${rel.name}`}
                        disabled={
                          deletingKey ===
                          `${rel.namespace || "tenant-default"}/${rel.name}`
                        }
                        onClick={() => handleDeleteRelease(rel)}
                        className="p-1.5 rounded bg-red-500/10 hover:bg-red-500/20 text-red-300 border border-red-500/30 transition-colors cursor-pointer disabled:opacity-40"
                      >
                        <Trash2 className="h-3 w-3" />
                      </button>
                    </div>
                  </div>
                </div>
              )
            })}
          </div>
        )}
        {deleteError && (
          <div className="mt-3 px-4 py-2.5 rounded-xl bg-red-500/10 border border-red-500/20 text-xs text-red-300">
            {deleteError}
          </div>
        )}
        {releaseBanner && (
          <div className="mt-3 px-4 py-2.5 rounded-xl bg-emerald-500/10 border border-emerald-500/20 text-xs text-emerald-300 flex items-center justify-between gap-3">
            <span>{releaseBanner.message}</span>
            <div className="flex items-center gap-3 shrink-0">
              {onNavigateToPreview && releaseBanner.prId && (
                <button
                  type="button"
                  onClick={onNavigateToPreview}
                  className="px-2.5 py-1 rounded-lg bg-purple-600 hover:bg-purple-500 text-white font-semibold transition-colors cursor-pointer"
                >
                  Go to PR →
                </button>
              )}
              {releaseBanner.prUrl && (
                <a
                  href={releaseBanner.prUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="text-emerald-200 hover:text-white underline font-semibold"
                >
                  View on Forge
                </a>
              )}
              <button
                type="button"
                onClick={() => setReleaseBanner(null)}
                className="text-emerald-400 hover:text-emerald-200 underline cursor-pointer"
              >
                Dismiss
              </button>
            </div>
          </div>
        )}
      </div>

      {/* Real-time SSE CRD Event Stream Console */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 border-b border-slate-800 pb-4">
          <div className="flex items-center space-x-3">
            <Radio className="h-5 w-5 text-indigo-400 animate-pulse" />
            <div>
              <h3 className="text-base font-semibold text-white">
                Live Reconciler Event Stream
              </h3>
              <p className="text-xs text-slate-400">
                kube-rs CRD condition transitions for Flux &amp; PKO v2 Stacks
              </p>
            </div>
          </div>

          {/* Console Toolbar */}
          <div className="flex items-center space-x-3">
            <div className="flex items-center space-x-1 bg-slate-950 border border-slate-800 rounded-lg p-1">
              <Filter className="h-3.5 w-3.5 text-slate-500 ml-2" />
              <select
                aria-label="Filter events by CRD kind"
                value={filterKind}
                onChange={(e) => setFilterKind(e.target.value)}
                className="bg-transparent border-0 text-xs text-slate-300 focus:outline-none focus:ring-0 py-0.5 px-2"
              >
                <option value="all">All CRDs</option>
                <option value="GitRepository">GitRepository</option>
                <option value="Kustomization">Kustomization</option>
                <option value="HelmRelease">HelmRelease</option>
                <option value="Stack">Stack</option>
                <option value="Canary">Canary</option>
              </select>
            </div>

            <button
              onClick={() => setIsPaused(!isPaused)}
              className={`flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors ${
                isPaused
                  ? "bg-amber-500/10 text-amber-400 border-amber-500/30 hover:bg-amber-500/20"
                  : "bg-slate-800 text-slate-300 border-slate-700 hover:bg-slate-700"
              }`}
            >
              {isPaused ? (
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
              onClick={handleClear}
              className="p-1.5 text-slate-400 hover:text-red-400 bg-slate-950 hover:bg-slate-800 border border-slate-800 rounded-lg transition-colors"
              title="Clear event logs"
            >
              <Trash2 className="h-4 w-4" />
            </button>
          </div>
        </div>

        {/* Timeline Log Window */}
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

      {inspectedStack && (
        <StackInspectorModal
          namespace={inspectedStack.namespace}
          name={inspectedStack.name}
          kind={inspectedStack.kind}
          onClose={() => setInspectedStack(null)}
        />
      )}
    </div>
  )
}
