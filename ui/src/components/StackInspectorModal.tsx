import React, { useCallback, useEffect, useRef, useState } from "react"
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
  Trash2,
  Unlock,
  ChevronDown,
  ChevronRight,
} from "lucide-react"
import { InfraResourceKind, StackDiagnostics } from "../types/api"
import {
  forceDeleteStack,
  getReleaseDiagnostics,
  getStackDiagnostics,
  restartStackWorkspace,
  unlockStack,
} from "../lib/api"
import { cn, isStackSshSupported } from "../lib/utils"
import { errorMessage } from "../lib/errors"
import { useAuthz } from "../lib/authz-context"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "./ui/dialog"
import { Button } from "./ui/button"
import { ConfirmDialog } from "./ui/confirm-dialog"
import { TruncatedText } from "./ui/truncated-text"

const MANAGEMENT_NAMESPACES = new Set(["flux-system", "keel-system"])

/** Section labels differ for PKO `Stack`s vs Flux releases (UI-11). */
// eslint-disable-next-line react-refresh/only-export-components

// Load the SSH terminal (wterm WASM core) only when a session is opened.
const VmSshModal = React.lazy(() =>
  import("./VmSshModal").then((m) => ({ default: m.VmSshModal }))
)

export function inspectorLabels(kind: "stack" | "release") {
  return kind === "release"
    ? {
        resource: "Release",
        conditions: "Release Conditions",
        events: "Release Events",
        logs: "Application Pod Logs",
        noEvents: "No recent events for this release.",
        noLogs:
          "No application pod logs available (no matching pod found or it has not started).",
      }
    : {
        resource: "Stack",
        conditions: "Stack Conditions",
        events: "Recent Kubernetes Events",
        logs: "Workspace Pod Logs",
        noEvents: "No recent events for this stack or workspace.",
        noLogs:
          "No pod logs available yet (workspace pod may still be initializing or completed).",
      }
}

export interface StackInspectorModalProps {
  namespace: string
  name: string
  kind?: "stack" | "release"
  tenant?: string
  provider?: string
  /** Server classification of the stack; only `vm` stacks offer SSH. */
  resourceKind?: InfraResourceKind
  onClose: () => void
  onRestarted?: () => void
  onForceDeleted?: () => void
  onUnlocked?: () => void
  onOpenSsh?: () => void
}

export const StackInspectorModal: React.FC<StackInspectorModalProps> = ({
  namespace,
  name,
  kind = "stack",
  tenant,
  provider,
  resourceKind,
  onClose,
  onRestarted,
  onForceDeleted,
  onUnlocked,
  onOpenSsh,
}) => {
  const [diag, setDiag] = useState<StackDiagnostics | null>(null)
  const [loading, setLoading] = useState(true)
  const [restarting, setRestarting] = useState(false)
  const [unlocking, setUnlocking] = useState(false)
  const [confirmingForceDelete, setConfirmingForceDelete] = useState(false)
  const [forceDeleting, setForceDeleting] = useState(false)
  const [autoRefresh, setAutoRefresh] = useState(true)
  const [isFullscreen, setIsFullscreen] = useState(false)
  const [banner, setBanner] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [isSshModalOpen, setIsSshModalOpen] = useState(false)
  const [showAdvanced, setShowAdvanced] = useState(false)
  const pendingTimeoutsRef = useRef<Set<number>>(new Set())
  const authz = useAuthz()
  const labels = inspectorLabels(kind)
  const isManagementResource =
    MANAGEMENT_NAMESPACES.has(namespace) || name === "management-cluster-core"
  // Restart / unlock / force-delete need tenant-admin (cluster admin for management-plane stacks).
  const canOperate = isManagementResource
    ? authz.isAdmin
    : authz.canAdminTenant(tenant || namespace)
  const operateDeniedTitle = isManagementResource
    ? "Cluster administrator privileges required for management-plane stacks"
    : "Tenant administrator privileges required"

  useEffect(() => {
    const timeouts = pendingTimeoutsRef.current
    return () => {
      timeouts.forEach((id) => window.clearTimeout(id))
      timeouts.clear()
    }
  }, [])
  const isProtectedStack = isManagementResource
  const isStackLocked =
    kind === "stack" &&
    (diag?.logs?.some((line) =>
      line.includes("the stack is currently locked by")
    ) ??
      false)
  const isTerminating =
    kind === "stack" && Boolean(diag?.metadata?.deletionTimestamp)
  const isWaitingForWorkspace =
    kind === "stack" &&
    (diag?.conditions?.some(
      (c) =>
        c.type === "Reconciling" &&
        c.status === "True" &&
        (c.message?.toLowerCase().includes("workspace") ||
          c.reason === "StackProcessing")
    ) ??
      false)

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
      setError(errorMessage(err, "Failed to load diagnostics"))
    } finally {
      setLoading(false)
    }
  }, [fetchDiag])

  const scheduleDiagnosticsRefresh = useCallback(
    (delayMs: number) => {
      const id = window.setTimeout(() => {
        pendingTimeoutsRef.current.delete(id)
        void loadDiagnostics()
      }, delayMs)
      pendingTimeoutsRef.current.add(id)
    },
    [loadDiagnostics]
  )

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
          setError(errorMessage(err, "Failed to load diagnostics"))
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
      scheduleDiagnosticsRefresh(1500)
    } catch (err: unknown) {
      setError(errorMessage(err, "Failed to restart workspace"))
    } finally {
      setRestarting(false)
    }
  }

  const handleForceDelete = async () => {
    setForceDeleting(true)
    setError(null)
    setBanner(null)
    try {
      const res = await forceDeleteStack(namespace, name)
      setBanner(res.message)
      ;(onForceDeleted ?? onRestarted)?.()
    } catch (err: unknown) {
      setError(errorMessage(err, "Failed to force-delete stack"))
    } finally {
      setForceDeleting(false)
    }
  }

  const handleUnlock = async () => {
    setUnlocking(true)
    setError(null)
    setBanner(null)
    try {
      const res = await unlockStack(namespace, name)
      setBanner(res.message)
      ;(onUnlocked ?? onRestarted)?.()
      scheduleDiagnosticsRefresh(1500)
    } catch (err: unknown) {
      setError(errorMessage(err, "Failed to unlock stack"))
    } finally {
      setUnlocking(false)
    }
  }

  const actionBusy = restarting || forceDeleting || unlocking

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose()
      }}
    >
      <DialogContent
        size="full"
        hideClose
        aria-label={`${labels.resource} Inspector ${namespace}/${name}`}
        className={cn(
          isFullscreen &&
            "w-screen h-dvh max-h-dvh sm:max-w-none rounded-none border-0"
        )}
      >
        {/* Header */}
        <div className="flex flex-col gap-3 px-4 sm:px-6 py-4 border-b border-slate-800 bg-slate-950/60">
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-2">
                <Activity className="h-5 w-5 text-indigo-400 shrink-0" />
                <DialogTitle
                  className="min-w-0 truncate font-mono"
                  title={`${namespace}/${name}`}
                >
                  {namespace}/{name}
                </DialogTitle>
                {diag?.workspacePhase && (
                  <span
                    className={`text-[10px] px-2 py-0.5 rounded-full font-semibold border shrink-0 ${
                      diag.workspacePhase === "Running"
                        ? "bg-emerald-500/10 text-emerald-400 border-emerald-500/20"
                        : "bg-blue-500/10 text-blue-400 border-blue-500/20"
                    }`}
                  >
                    Pod: {diag.workspacePhase}
                  </span>
                )}
              </div>
              <DialogDescription className="mt-0.5">
                {kind === "release"
                  ? "Live Flux HelmRelease / Kustomization conditions, Kubernetes events, and application pod logs"
                  : "Live PKO v2 Stack conditions, Kubernetes events, and workspace pod logs"}
              </DialogDescription>
            </div>

            <div className="flex items-center gap-1 shrink-0">
              <Button
                variant="ghost"
                size="icon-sm"
                onClick={() => setIsFullscreen((v) => !v)}
                aria-label={
                  isFullscreen ? "Exit fullscreen" : "Expand fullscreen"
                }
                title={isFullscreen ? "Exit fullscreen" : "Expand fullscreen"}
              >
                {isFullscreen ? (
                  <Minimize2 className="h-4 w-4" />
                ) : (
                  <Maximize2 className="h-4 w-4" />
                )}
              </Button>
              <Button
                variant="ghost"
                size="icon-sm"
                onClick={onClose}
                aria-label="Close inspector"
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <Button
              variant={autoRefresh ? "subtle" : "secondary"}
              aria-pressed={autoRefresh}
              onClick={() => setAutoRefresh((v) => !v)}
            >
              {autoRefresh ? "Auto: 5s" : "Auto: Off"}
            </Button>
            <Button
              variant="secondary"
              onClick={loadDiagnostics}
              disabled={loading}
            >
              <RefreshCw
                className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
              />
              <span>Refresh</span>
            </Button>
            {kind === "stack" &&
              (() => {
                const sshSupported = isStackSshSupported({
                  provider,
                  resourceKind,
                })
                return (
                  <button
                    type="button"
                    data-testid="inspector-ssh-btn"
                    disabled={!sshSupported}
                    title={
                      sshSupported
                        ? "SSH / Connect"
                        : "SSH is not supported for this infrastructure"
                    }
                    onClick={() => {
                      if (onOpenSsh) {
                        onOpenSsh()
                      } else {
                        setIsSshModalOpen(true)
                      }
                    }}
                    className="flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-semibold bg-emerald-500/15 hover:bg-emerald-500/25 text-emerald-300 border border-emerald-500/30 transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                  >
                    <Terminal className="h-3.5 w-3.5" />
                    <span>SSH / Connect</span>
                  </button>
                )
              })()}
            {kind === "stack" && (
              <button
                type="button"
                data-testid="inspector-unlock-btn"
                onClick={handleUnlock}
                disabled={!canOperate || actionBusy}
                title={
                  canOperate
                    ? "Cancel a stale Pulumi state lock and request reconciliation"
                    : operateDeniedTitle
                }
                className="flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-semibold bg-sky-500/15 hover:bg-sky-500/25 text-sky-300 border border-sky-500/30 transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
              >
                <Unlock className="h-3.5 w-3.5" />
                <span>{unlocking ? "Unlocking..." : "Unlock Stack"}</span>
              </button>
            )}
            {kind === "stack" && !isProtectedStack && (
              <button
                type="button"
                data-testid="inspector-force-delete-btn"
                onClick={() => setConfirmingForceDelete(true)}
                disabled={!canOperate || actionBusy}
                title={
                  canOperate
                    ? "Clear finalizers and force-delete a stuck or terminating Stack"
                    : operateDeniedTitle
                }
                className="flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-semibold bg-red-500/15 hover:bg-red-500/25 text-red-300 border border-red-500/30 transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
              >
                <Trash2 className="h-3.5 w-3.5" />
                <span>
                  {forceDeleting ? "Force Deleting..." : "Force Delete"}
                </span>
              </button>
            )}
            {kind === "stack" && !canOperate && (
              <span
                data-testid="inspector-readonly-note"
                className="text-[11px] text-slate-500"
              >
                Read-only: {operateDeniedTitle.toLowerCase()} to run operational
                actions.
              </span>
            )}
          </div>
        </div>

        {/* Body */}
        <div
          className={`flex-1 min-h-0 p-4 sm:p-6 ${
            isFullscreen
              ? "flex flex-col gap-5 overflow-hidden"
              : "overflow-y-auto space-y-5"
          }`}
        >
          {isTerminating && (
            <div
              data-testid="inspector-terminating-banner"
              className="shrink-0 p-3 rounded-xl bg-orange-500/10 border border-orange-500/40 text-xs text-orange-200 flex flex-wrap items-center justify-between gap-3"
            >
              <div className="flex items-center space-x-2">
                <AlertTriangle className="h-4 w-4 text-orange-400 shrink-0" />
                <span>
                  {isWaitingForWorkspace
                    ? "This stack is terminating and waiting for its workspace pod to become ready before running pulumi destroy. Try Restart Workspace before resorting to Force Delete."
                    : "This stack is terminating while Pulumi destroys its cloud resources. It will be removed automatically once finalized."}
                </span>
              </div>
              {isWaitingForWorkspace && canOperate && (
                <button
                  type="button"
                  onClick={handleRestart}
                  disabled={actionBusy}
                  className="flex items-center space-x-1.5 px-2.5 py-1 rounded-lg bg-amber-600 hover:bg-amber-500 text-white font-semibold cursor-pointer disabled:opacity-50"
                >
                  <RefreshCw
                    className={`h-3.5 w-3.5 ${restarting ? "animate-spin" : ""}`}
                  />
                  <span>
                    {restarting ? "Restarting..." : "Restart Workspace"}
                  </span>
                </button>
              )}
            </div>
          )}
          {isStackLocked && (
            <div className="shrink-0 p-3 rounded-xl bg-amber-500/10 border border-amber-500/40 text-xs text-amber-200 flex flex-wrap items-center justify-between gap-3">
              <div className="flex items-center space-x-2">
                <AlertTriangle className="h-4 w-4 text-amber-400 shrink-0" />
                <span>
                  A stale Pulumi state lock was detected in the workspace logs.
                  Unlock the stack to cancel the stale lock and trigger
                  reconciliation.
                </span>
              </div>
              <button
                type="button"
                onClick={handleUnlock}
                disabled={!canOperate || actionBusy}
                title={canOperate ? undefined : operateDeniedTitle}
                className="flex items-center space-x-1.5 px-2.5 py-1 rounded-lg bg-sky-600 hover:bg-sky-500 text-white font-semibold cursor-pointer disabled:opacity-50"
              >
                <Unlock className="h-3.5 w-3.5" />
                <span>{unlocking ? "Unlocking..." : "Unlock Now"}</span>
              </button>
            </div>
          )}
          <ConfirmDialog
            open={confirmingForceDelete && canOperate}
            onOpenChange={setConfirmingForceDelete}
            title="Force delete this Stack?"
            description={
              <>
                Clear finalizers and force-delete{" "}
                <code className="font-mono text-white">
                  {namespace}/{name}
                </code>
                ? Pulumi will not destroy any remaining cloud resources.
                {isWaitingForWorkspace
                  ? " This stack is currently waiting on workspace readiness — try Restart Workspace first."
                  : !isTerminating
                    ? " This stack is not currently terminating."
                    : ""}
              </>
            }
            confirmLabel="Confirm Force Delete"
            confirmTestId="confirm-force-delete-btn"
            destructive
            onConfirm={() => {
              setConfirmingForceDelete(false)
              void handleForceDelete()
            }}
          />

          {banner && (
            <div className="shrink-0 p-3 rounded-xl bg-emerald-500/10 border border-emerald-500/30 text-xs text-emerald-300 flex items-center space-x-2">
              <CheckCircle2 className="h-4 w-4 shrink-0" />
              <span>{banner}</span>
            </div>
          )}

          {error && (
            <div
              role="alert"
              className="shrink-0 p-3 rounded-xl bg-red-500/10 border border-red-500/30 text-xs text-red-300 flex items-center space-x-2"
            >
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
                    <TruncatedText
                      text={diag.metadata.uid || "—"}
                      className="text-slate-300"
                    />
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
              {labels.conditions}
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
                      <p className="text-xs text-slate-400 wrap-break-word">
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
                {labels.events} ({diag?.events.length ?? 0})
              </h4>
              {!diag || diag.events.length === 0 ? (
                <div
                  className={`p-3 rounded-xl bg-slate-950 border border-slate-800 text-xs text-slate-500 ${
                    isFullscreen ? "flex-1" : ""
                  }`}
                >
                  {labels.noEvents}
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
                        <p className="text-slate-300 wrap-break-word text-[11px]">
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
                    {labels.logs}{" "}
                    {diag?.workspacePod ? `(${diag.workspacePod})` : ""}
                  </span>
                </h4>
              </div>
              <div
                className={`bg-slate-950 border border-slate-800 rounded-xl p-3 font-mono text-[11px] text-slate-300 overflow-y-auto whitespace-pre-wrap wrap-break-word ${
                  isFullscreen ? "flex-1 min-h-0" : "max-h-72"
                }`}
              >
                {!diag || diag.logs.length === 0 ? (
                  <span className="text-slate-500">{labels.noLogs}</span>
                ) : (
                  diag.logs.join("\n")
                )}
              </div>
            </div>
          </div>

          {kind === "stack" && (
            <div className="shrink-0 rounded-xl border border-slate-800 bg-slate-950/60 overflow-hidden">
              <button
                type="button"
                data-testid="inspector-advanced-toggle"
                onClick={() => setShowAdvanced((prev) => !prev)}
                className="w-full flex items-center justify-between rounded-xl px-4 py-2.5 text-xs font-semibold uppercase tracking-wider text-slate-400 hover:text-slate-200 hover:bg-slate-900/60 transition-colors cursor-pointer focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-indigo-400"
              >
                <span>Advanced</span>
                {showAdvanced ? (
                  <ChevronDown className="h-4 w-4" />
                ) : (
                  <ChevronRight className="h-4 w-4" />
                )}
              </button>
              {showAdvanced && (
                <div className="px-4 py-3 border-t border-slate-800 flex flex-wrap items-center justify-between gap-3">
                  <div className="space-y-0.5">
                    <p className="text-xs font-medium text-slate-200">
                      Restart Workspace Pod
                    </p>
                    <p className="text-[11px] text-slate-400">
                      Triggers a Kubernetes rollout restart on the{" "}
                      <code className="font-mono text-slate-300">
                        {name}-workspace
                      </code>{" "}
                      StatefulSet if the workspace pod is wedged or stale.
                    </p>
                  </div>
                  <button
                    type="button"
                    data-testid="inspector-restart-workspace-btn"
                    onClick={handleRestart}
                    disabled={!canOperate || actionBusy}
                    title={canOperate ? undefined : operateDeniedTitle}
                    className="flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-semibold bg-amber-500/15 hover:bg-amber-500/25 text-amber-300 border border-amber-500/30 transition-colors cursor-pointer disabled:opacity-50"
                  >
                    <RotateCw
                      className={`h-3.5 w-3.5 ${restarting ? "animate-spin" : ""}`}
                    />
                    <span>
                      {restarting ? "Restarting..." : "Restart Workspace"}
                    </span>
                  </button>
                </div>
              )}
            </div>
          )}
        </div>
        {isSshModalOpen && (
          <React.Suspense fallback={null}>
            <VmSshModal
              stackName={name}
              namespace={namespace}
              tenant={tenant || "default"}
              provider={provider}
              onClose={() => setIsSshModalOpen(false)}
            />
          </React.Suspense>
        )}
      </DialogContent>
    </Dialog>
  )
}
