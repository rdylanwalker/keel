import React, { useCallback, useEffect, useState } from "react"
import {
  X,
  RefreshCw,
  RotateCw,
  Terminal,
  AlertTriangle,
  CheckCircle2,
  Clock,
  Activity,
  Maximize2,
  Minimize2,
} from "lucide-react"
import { StackDiagnostics } from "../types/api"
import {
  getReleaseDiagnostics,
  getStackDiagnostics,
  restartStackWorkspace,
} from "../lib/api"

interface StackInspectorModalProps {
  namespace: string
  name: string
  kind?: "stack" | "release"
  onClose: () => void
  onRestarted?: () => void
}

export const StackInspectorModal: React.FC<StackInspectorModalProps> = ({
  namespace,
  name,
  kind = "stack",
  onClose,
  onRestarted,
}) => {
  const [diag, setDiag] = useState<StackDiagnostics | null>(null)
  const [loading, setLoading] = useState(true)
  const [restarting, setRestarting] = useState(false)
  const [autoRefresh, setAutoRefresh] = useState(true)
  const [isFullscreen, setIsFullscreen] = useState(false)
  const [banner, setBanner] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const fetchDiag = useCallback(
    () =>
      kind === "release"
        ? getReleaseDiagnostics(namespace, name)
        : getStackDiagnostics(namespace, name),
    [kind, namespace, name]
  )

  const loadDiagnostics = useCallback(async () => {
    try {
      const data = await fetchDiag()
      setDiag(data)
      setError(null)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to load diagnostics"
      )
    } finally {
      setLoading(false)
    }
  }, [fetchDiag])

  useEffect(() => {
    let cancelled = false
    fetchDiag()
      .then((data) => {
        if (!cancelled) {
          setDiag(data)
          setError(null)
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(
            err instanceof Error ? err.message : "Failed to load diagnostics"
          )
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
  }, [fetchDiag])

  useEffect(() => {
    if (!autoRefresh) return
    const timer = window.setInterval(loadDiagnostics, 5000)
    return () => window.clearInterval(timer)
  }, [autoRefresh, loadDiagnostics])

  const handleRestart = async () => {
    setRestarting(true)
    setError(null)
    setBanner(null)
    try {
      const res = await restartStackWorkspace(namespace, name)
      setBanner(res.message)
      onRestarted?.()
      window.setTimeout(loadDiagnostics, 1500)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to restart workspace"
      )
    } finally {
      setRestarting(false)
    }
  }

  return (
    <div
      className={`fixed inset-0 z-50 flex items-center justify-center bg-black/75 backdrop-blur-sm ${
        isFullscreen ? "p-0" : "p-4"
      }`}
      role="dialog"
      aria-modal="true"
      aria-label={`Stack Inspector ${namespace}/${name}`}
    >
      <div
        className={`w-full flex flex-col bg-slate-900 border border-slate-700/80 shadow-2xl overflow-hidden ${
          isFullscreen
            ? "h-screen max-w-none rounded-none border-0"
            : "max-w-6xl max-h-[92vh] rounded-2xl"
        }`}
      >
        {/* Header */}
        <div className="flex flex-wrap items-center justify-between gap-3 px-6 py-4 border-b border-slate-800 bg-slate-950/60">
          <div className="min-w-0">
            <div className="flex items-center space-x-2">
              <Activity className="h-5 w-5 text-indigo-400 shrink-0" />
              <h3 className="text-base font-bold text-white font-mono truncate">
                {namespace}/{name}
              </h3>
              {diag?.workspacePhase && (
                <span
                  className={`text-[10px] px-2 py-0.5 rounded-full font-semibold border ${
                    diag.workspacePhase === "Running"
                      ? "bg-emerald-500/10 text-emerald-400 border-emerald-500/20"
                      : "bg-blue-500/10 text-blue-400 border-blue-500/20"
                  }`}
                >
                  Pod: {diag.workspacePhase}
                </span>
              )}
            </div>
            <p className="text-xs text-slate-400 mt-0.5">
              {kind === "release"
                ? "Live Flux HelmRelease / Kustomization conditions, Kubernetes events, and pod logs"
                : "Live PKO v2 Stack conditions, Kubernetes events, and workspace pod logs"}
            </p>
          </div>

          <div className="flex items-center space-x-2">
            <button
              type="button"
              onClick={() => setAutoRefresh((v) => !v)}
              className={`px-2.5 py-1.5 rounded-lg text-xs font-medium border transition-colors cursor-pointer ${
                autoRefresh
                  ? "bg-indigo-600/20 text-indigo-300 border-indigo-500/40"
                  : "bg-slate-800 text-slate-400 border-slate-700"
              }`}
            >
              {autoRefresh ? "Auto: 5s" : "Auto: Off"}
            </button>
            <button
              type="button"
              onClick={loadDiagnostics}
              disabled={loading}
              className="flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer disabled:opacity-50"
            >
              <RefreshCw
                className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
              />
              <span>Refresh</span>
            </button>
            {kind === "stack" && (
              <button
                type="button"
                onClick={handleRestart}
                disabled={restarting}
                className="flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-semibold bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 transition-colors cursor-pointer disabled:opacity-50"
              >
                <RotateCw
                  className={`h-3.5 w-3.5 ${restarting ? "animate-spin" : ""}`}
                />
                <span>{restarting ? "Restarting..." : "Rollout Restart"}</span>
              </button>
            )}
            <button
              type="button"
              onClick={() => setIsFullscreen((v) => !v)}
              aria-label={
                isFullscreen ? "Exit fullscreen" : "Expand fullscreen"
              }
              title={isFullscreen ? "Exit fullscreen" : "Expand fullscreen"}
              className="p-1.5 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              {isFullscreen ? (
                <Minimize2 className="h-4 w-4" />
              ) : (
                <Maximize2 className="h-4 w-4" />
              )}
            </button>
            <button
              type="button"
              onClick={onClose}
              aria-label="Close inspector"
              className="p-1.5 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              <X className="h-4 w-4" />
            </button>
          </div>
        </div>

        {/* Body */}
        <div
          className={`flex-1 min-h-0 p-6 ${
            isFullscreen
              ? "flex flex-col gap-5 overflow-hidden"
              : "overflow-y-auto space-y-5"
          }`}
        >
          {banner && (
            <div className="shrink-0 p-3 rounded-xl bg-emerald-500/10 border border-emerald-500/30 text-xs text-emerald-300 flex items-center space-x-2">
              <CheckCircle2 className="h-4 w-4 shrink-0" />
              <span>{banner}</span>
            </div>
          )}

          {error && (
            <div className="shrink-0 p-3 rounded-xl bg-red-500/10 border border-red-500/30 text-xs text-red-300 flex items-center space-x-2">
              <AlertTriangle className="h-4 w-4 shrink-0" />
              <span>{error}</span>
            </div>
          )}

          {/* Resource Metadata */}
          {diag?.metadata && (
            <div className="space-y-2 shrink-0">
              <h4 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
                Resource Metadata
              </h4>
              <div className="p-4 rounded-xl bg-slate-950 border border-slate-800 space-y-3 text-xs">
                <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-3 font-mono">
                  <div>
                    <span className="text-slate-500 block text-[10px] uppercase">
                      Kind / API
                    </span>
                    <span className="text-slate-200">
                      {diag.metadata.kind || "Resource"} (
                      {diag.metadata.apiVersion || "—"})
                    </span>
                  </div>
                  <div>
                    <span className="text-slate-500 block text-[10px] uppercase">
                      Created
                    </span>
                    <span className="text-slate-200">
                      {diag.metadata.creationTimestamp
                        ? new Date(
                            diag.metadata.creationTimestamp
                          ).toLocaleString()
                        : "—"}
                    </span>
                  </div>
                  <div className="sm:col-span-2">
                    <span className="text-slate-500 block text-[10px] uppercase">
                      UID
                    </span>
                    <span className="text-slate-300 break-all">
                      {diag.metadata.uid || "—"}
                    </span>
                  </div>
                </div>

                {diag.metadata.specSummary &&
                  Object.keys(diag.metadata.specSummary).length > 0 && (
                    <div className="pt-2 border-t border-slate-800/80 grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-2 font-mono">
                      {Object.entries(diag.metadata.specSummary).map(
                        ([k, v]) => (
                          <div key={k} className="truncate">
                            <span className="text-slate-500">{k}: </span>
                            <span className="text-indigo-300" title={v}>
                              {v}
                            </span>
                          </div>
                        )
                      )}
                    </div>
                  )}

                {diag.metadata.labels &&
                  Object.keys(diag.metadata.labels).length > 0 && (
                    <div className="pt-2 border-t border-slate-800/80 flex flex-wrap items-center gap-1.5">
                      <span className="text-[10px] uppercase text-slate-500 mr-1">
                        Labels:
                      </span>
                      {Object.entries(diag.metadata.labels).map(([k, v]) => (
                        <span
                          key={k}
                          className="px-2 py-0.5 rounded bg-slate-900 border border-slate-800 font-mono text-[11px] text-slate-300"
                        >
                          <span className="text-slate-500">{k}=</span>
                          {v}
                        </span>
                      ))}
                    </div>
                  )}

                {diag.metadata.annotations &&
                  Object.keys(diag.metadata.annotations).length > 0 && (
                    <div className="pt-2 border-t border-slate-800/80 flex flex-wrap items-center gap-1.5">
                      <span className="text-[10px] uppercase text-slate-500 mr-1">
                        Annotations:
                      </span>
                      {Object.entries(diag.metadata.annotations).map(
                        ([k, v]) => (
                          <span
                            key={k}
                            className="px-2 py-0.5 rounded bg-slate-900 border border-slate-800 font-mono text-[11px] text-slate-300 max-w-full truncate"
                            title={`${k}=${v}`}
                          >
                            <span className="text-slate-500">{k}=</span>
                            {v}
                          </span>
                        )
                      )}
                    </div>
                  )}
              </div>
            </div>
          )}

          {/* Conditions */}
          <div
            className={`space-y-2 ${
              isFullscreen ? "shrink-0 max-h-[28vh] overflow-y-auto pr-1" : ""
            }`}
          >
            <h4 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
              Stack Conditions
            </h4>
            {!diag || diag.conditions.length === 0 ? (
              <div className="p-3 rounded-xl bg-slate-950 border border-slate-800 text-xs text-slate-500">
                No conditions reported yet.
              </div>
            ) : (
              <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                {diag.conditions.map((cond) => (
                  <div
                    key={cond.type}
                    className="p-3 rounded-xl bg-slate-950 border border-slate-800 space-y-1"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-xs font-bold text-white">
                        {cond.type}
                      </span>
                      <span
                        className={`text-[10px] px-2 py-0.5 rounded-full font-semibold ${
                          cond.status === "True" && cond.type === "Ready"
                            ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                            : cond.type === "Reconciling" &&
                                cond.status === "True"
                              ? "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                              : "bg-slate-800 text-slate-300 border border-slate-700"
                        }`}
                      >
                        {cond.status} · {cond.reason}
                      </span>
                    </div>
                    {cond.message && (
                      <p className="text-xs text-slate-400 break-words">
                        {cond.message}
                      </p>
                    )}
                  </div>
                ))}
              </div>
            )}
          </div>

          {/* Events & Workspace Pod Logs */}
          <div
            className={
              isFullscreen
                ? "flex-1 min-h-0 grid grid-cols-1 lg:grid-cols-2 gap-5"
                : "space-y-5"
            }
          >
            {/* Events */}
            <div
              className={`space-y-2 ${
                isFullscreen ? "flex flex-col min-h-0" : ""
              }`}
            >
              <h4 className="shrink-0 text-xs font-semibold uppercase tracking-wider text-slate-400">
                Recent Kubernetes Events ({diag?.events.length ?? 0})
              </h4>
              {!diag || diag.events.length === 0 ? (
                <div
                  className={`p-3 rounded-xl bg-slate-950 border border-slate-800 text-xs text-slate-500 ${
                    isFullscreen ? "flex-1" : ""
                  }`}
                >
                  No recent events for this stack or workspace.
                </div>
              ) : (
                <div
                  className={`bg-slate-950 border border-slate-800 rounded-xl p-3 space-y-2 overflow-y-auto font-mono text-xs ${
                    isFullscreen ? "flex-1 min-h-0" : "max-h-48"
                  }`}
                >
                  {diag.events.map((ev, idx) => (
                    <div
                      key={`${ev.reason}-${idx}`}
                      className="flex items-start justify-between gap-3 p-2 rounded-lg bg-slate-900/60 border border-slate-800/70"
                    >
                      <div className="min-w-0 flex-1 space-y-0.5">
                        <div className="flex items-center space-x-2">
                          <span
                            className={`text-[10px] px-1.5 py-0.5 rounded font-bold ${
                              ev.eventType === "Warning"
                                ? "bg-amber-500/10 text-amber-400 border border-amber-500/20"
                                : "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                            }`}
                          >
                            {ev.reason || ev.eventType}
                          </span>
                          {ev.count && ev.count > 1 && (
                            <span className="text-[10px] text-slate-500">
                              ×{ev.count}
                            </span>
                          )}
                        </div>
                        <p className="text-slate-300 break-words text-[11px]">
                          {ev.message}
                        </p>
                      </div>
                      {ev.lastSeen && (
                        <span className="text-[10px] text-slate-500 shrink-0 flex items-center gap-1">
                          <Clock className="h-3 w-3" />
                          {new Date(ev.lastSeen).toLocaleTimeString()}
                        </span>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </div>

            {/* Workspace Pod Logs */}
            <div
              className={`space-y-2 ${
                isFullscreen ? "flex flex-col min-h-0" : ""
              }`}
            >
              <div className="shrink-0 flex items-center justify-between">
                <h4 className="text-xs font-semibold uppercase tracking-wider text-slate-400 flex items-center space-x-1.5">
                  <Terminal className="h-3.5 w-3.5" />
                  <span>
                    Workspace Pod Logs{" "}
                    {diag?.workspacePod ? `(${diag.workspacePod})` : ""}
                  </span>
                </h4>
              </div>
              <div
                className={`bg-slate-950 border border-slate-800 rounded-xl p-3 font-mono text-[11px] text-slate-300 overflow-y-auto whitespace-pre-wrap break-words ${
                  isFullscreen ? "flex-1 min-h-0" : "max-h-72"
                }`}
              >
                {!diag || diag.logs.length === 0 ? (
                  <span className="text-slate-500">
                    No pod logs available yet (workspace pod may still be
                    initializing or completed).
                  </span>
                ) : (
                  diag.logs.join("\n")
                )}
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
