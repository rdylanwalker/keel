import React, { useEffect, useState } from "react"
import {
  GitPullRequest,
  CheckCircle2,
  AlertTriangle,
  Loader2,
  ExternalLink,
  PlusCircle,
  RefreshCcw,
  MinusCircle,
  AlertOctagon,
  ShieldCheck,
  GitMerge,
  Layers,
  XCircle,
  RotateCcw,
} from "lucide-react"
import { PreviewReport, Proposal, ResourceDiff } from "../types/api"
import { GitDiffViewer } from "./GitDiffViewer"
import {
  getProposalPreview,
  getProposals,
  mergeProposal,
  rejectProposal,
  reopenProposal,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"

const DEFAULT_PROPOSALS: Proposal[] = [
  {
    id: "42",
    title: "Deploy PostgreSQL database for tenant-acme",
    branch: "keel/tenant-acme/db-pg-42",
    author: "dylan@acme.internal",
    tenant: "tenant-acme",
    status: "open",
    createdAt: "10 minutes ago",
    prUrl: "https://git.internal/keel-io/infra/pulls/42",
    previewStatus: "ready",
  },
  {
    id: "43",
    title: "Scale Proxmox worker pool nodes from 3 to 5",
    branch: "keel/tenant-globex/pve-scale-43",
    author: "infra-bot",
    tenant: "tenant-globex",
    status: "open",
    createdAt: "25 minutes ago",
    prUrl: "https://git.internal/keel-io/infra/pulls/43",
    previewStatus: "ready",
  },
]

const DEFAULT_PREVIEW: PreviewReport = {
  prUrl: "https://git.internal/keel-io/infra/pulls/42",
  policyStatus: "passed",
  summary: {
    create: 3,
    update: 2,
    delete: 0,
    replace: 1,
    same: 12,
    drift: 0,
    total: 18,
  },
  resources: [
    {
      urn: "urn:pulumi:prod::infra::kubernetes:core/v1:Namespace::tenant-acme",
      resource_type: "kubernetes:core/v1:Namespace",
      name: "tenant-acme",
      op: "Create",
      changed_fields: ["metadata.labels", "metadata.name"],
    },
    {
      urn: "urn:pulumi:prod::infra::kubernetes:helm.sh/v3:Release::pg-cluster",
      resource_type: "kubernetes:helm.sh/v3:Release",
      name: "pg-cluster",
      op: "Create",
      changed_fields: ["spec.values", "spec.chart"],
    },
    {
      urn: "urn:pulumi:prod::infra::kubernetes:core/v1:Secret::pg-credentials",
      resource_type: "kubernetes:core/v1:Secret",
      name: "pg-credentials",
      op: "Create",
      changed_fields: ["stringData.password"],
    },
    {
      urn: "urn:pulumi:prod::infra::proxmox:ve/vm:VirtualMachine::worker-node-04",
      resource_type: "proxmox:ve/vm:VirtualMachine",
      name: "worker-node-04",
      op: "Replace",
      changed_fields: ["cores", "memory", "storagePool"],
    },
    {
      urn: "urn:pulumi:prod::infra::kubernetes:networking.k8s.io/v1:Ingress::frontend-ingress",
      resource_type: "kubernetes:networking.k8s.io/v1:Ingress",
      name: "frontend-ingress",
      op: "Update",
      changed_fields: ["spec.rules[0].host"],
    },
    {
      urn: "urn:pulumi:prod::infra::kyverno.io/v1:ClusterPolicy::restrict-pulumi-providers",
      resource_type: "kyverno.io/v1:ClusterPolicy",
      name: "restrict-pulumi-providers",
      op: "Update",
      changed_fields: ["spec.rules[0].match"],
    },
  ],
}

export const PreviewInspectorView: React.FC = () => {
  const [proposals, setProposals] = useState<Proposal[]>(() =>
    isDemoMode() ? DEFAULT_PROPOSALS : []
  )
  const [selectedPR, setSelectedPR] = useState<Proposal | null>(() =>
    isDemoMode() ? DEFAULT_PROPOSALS[0] : null
  )
  const [preview, setPreview] = useState<PreviewReport | null>(() =>
    isDemoMode() ? DEFAULT_PREVIEW : null
  )
  const [loadingPRs, setLoadingPRs] = useState(false)
  const [loadingPreview, setLoadingPreview] = useState(false)
  const [merging, setMerging] = useState(false)
  const [rejecting, setRejecting] = useState(false)
  const [reopening, setReopening] = useState(false)
  const [mergeSuccess, setMergeSuccess] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [statusFilter, setStatusFilter] = useState<
    "all" | "open" | "merged" | "closed"
  >("all")

  useEffect(() => {
    let cancelled = false
    getProposals(undefined, statusFilter)
      .then((data) => {
        if (!cancelled) {
          if (data && data.length > 0) {
            const sorted = [...data].sort((a, b) => {
              if (a.status === "open" && b.status !== "open") return -1
              if (a.status !== "open" && b.status === "open") return 1
              return Number(b.id) - Number(a.id)
            })
            setProposals(sorted)
            setSelectedPR((prev) => {
              if (prev) {
                const fresh = sorted.find((p) => p.id === prev.id)
                if (fresh) return fresh
              }
              return sorted[0]
            })
          } else if (!isDemoMode()) {
            setProposals([])
            setSelectedPR(null)
            setPreview(null)
          }
        }
      })
      .catch((err) => {
        if (!cancelled) {
          const msg =
            err instanceof Error ? err.message : "Failed to load PR proposals"
          if (!(isDemoMode() && msg.includes("API error (401)"))) {
            setError(msg)
          }
          if (!isDemoMode()) {
            setProposals([])
            setSelectedPR(null)
            setPreview(null)
          }
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoadingPRs(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [statusFilter])

  useEffect(() => {
    if (!selectedPR) return
    let cancelled = false
    getProposalPreview(selectedPR.id)
      .then((report) => {
        if (!cancelled) {
          setError(null)
          setPreview(report)
        }
      })
      .catch((err) => {
        if (!cancelled) {
          const msg =
            err instanceof Error
              ? err.message
              : "Failed to load PKO v2 preview diff"
          if (!(isDemoMode() && msg.includes("API error (401)"))) {
            setError(msg)
          }
          if (isDemoMode()) {
            setPreview(DEFAULT_PREVIEW)
          } else {
            setPreview(null)
          }
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoadingPreview(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [selectedPR])

  const handleSelectPR = (pr: Proposal) => {
    setSelectedPR(pr)
    setMergeSuccess(null)
  }

  const handleMergePR = async () => {
    if (!selectedPR) return
    setMerging(true)
    setMergeSuccess(null)
    setError(null)
    try {
      const result = await mergeProposal(selectedPR.id)
      if (result.success) {
        setMergeSuccess(
          result.message || `PR #${selectedPR.id} successfully merged!`
        )
        // Update local state
        setProposals((prev) =>
          prev.map((p) =>
            p.id === selectedPR.id ? { ...p, status: "merged" } : p
          )
        )
        setSelectedPR((prev) => (prev ? { ...prev, status: "merged" } : null))
      } else {
        setError(result.message || "Failed to merge PR proposal")
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Merge operation failed")
    } finally {
      setMerging(false)
    }
  }

  const handleRejectPR = async () => {
    if (!selectedPR) return
    setRejecting(true)
    setMergeSuccess(null)
    setError(null)
    try {
      const result = await rejectProposal(selectedPR.id)
      if (result.success) {
        setMergeSuccess(
          result.message || `PR #${selectedPR.id} rejected and closed.`
        )
        setProposals((prev) =>
          prev.map((p) =>
            p.id === selectedPR.id ? { ...p, status: "closed" } : p
          )
        )
        setSelectedPR((prev) => (prev ? { ...prev, status: "closed" } : null))
      } else {
        setError(result.message || "Failed to reject PR proposal")
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Reject operation failed")
    } finally {
      setRejecting(false)
    }
  }

  const handleReopenPR = async () => {
    if (!selectedPR) return
    setReopening(true)
    setMergeSuccess(null)
    setError(null)
    try {
      const result = await reopenProposal(selectedPR.id)
      if (result.success) {
        setMergeSuccess(result.message || `PR #${selectedPR.id} reopened.`)
        setProposals((prev) =>
          prev.map((p) =>
            p.id === selectedPR.id ? { ...p, status: "open" } : p
          )
        )
        setSelectedPR((prev) => (prev ? { ...prev, status: "open" } : null))
      } else {
        setError(result.message || "Failed to reopen PR proposal")
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Reopen operation failed")
    } finally {
      setReopening(false)
    }
  }

  const renderOpBadge = (op: ResourceDiff["op"]) => {
    switch ((op ?? "").toLowerCase()) {
      case "create":
        return (
          <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-emerald-500/10 text-emerald-400 border border-emerald-500/20">
            <PlusCircle className="h-3 w-3" />
            <span>Create</span>
          </span>
        )
      case "update":
        return (
          <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-blue-500/10 text-blue-400 border border-blue-500/20">
            <RefreshCcw className="h-3 w-3" />
            <span>Update</span>
          </span>
        )
      case "delete":
        return (
          <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-red-500/10 text-red-400 border border-red-500/20">
            <MinusCircle className="h-3 w-3" />
            <span>Delete</span>
          </span>
        )
      case "replace":
        return (
          <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-amber-500/10 text-amber-400 border border-amber-500/20">
            <AlertOctagon className="h-3 w-3" />
            <span>Replace</span>
          </span>
        )
      case "drift":
        return (
          <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-amber-500/10 text-amber-400 border border-amber-500/20">
            <AlertOctagon className="h-3 w-3" />
            <span>Drift</span>
          </span>
        )
      default:
        return (
          <span className="inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-xs font-semibold bg-slate-800 text-slate-400">
            <span>{op}</span>
          </span>
        )
    }
  }

  return (
    <div className="space-y-6">
      {/* Top Header */}
      <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 bg-slate-900 border border-slate-800 p-6 rounded-2xl">
        <div>
          <h2 className="text-xl font-bold text-white flex items-center gap-2">
            <GitPullRequest className="h-6 w-6 text-indigo-400" />
            PR &amp; PKO v2 Preview Inspector
          </h2>
          <p className="text-xs text-slate-400 mt-1">
            Review proposed infrastructure diffs (
            <code className="text-slate-300">+ Create</code>,{" "}
            <code className="text-slate-300">~ Update</code>,{" "}
            <code className="text-slate-300">- Delete</code>,{" "}
            <code className="text-slate-300">! Replace</code>), inspect code
            changes, and merge or reject pull requests
          </p>
        </div>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-6 lg:h-[calc(100vh-14rem)] lg:min-h-140">
        {/* Left PR Selector List */}
        <div className="lg:col-span-4 flex flex-col lg:h-full min-h-0 space-y-4">
          <div className="flex items-center justify-between shrink-0">
            <h3 className="text-xs font-semibold text-slate-400 uppercase tracking-wider">
              Pull Requests ({proposals.length})
            </h3>
            {loadingPRs && (
              <Loader2 className="h-3.5 w-3.5 text-indigo-400 animate-spin" />
            )}
          </div>

          <div className="flex items-center gap-1.5 shrink-0">
            {(["all", "open", "merged", "closed"] as const).map((st) => (
              <button
                key={st}
                type="button"
                onClick={() => setStatusFilter(st)}
                className={`px-2.5 py-1 rounded-lg text-[11px] font-medium capitalize transition-colors cursor-pointer ${
                  statusFilter === st
                    ? "bg-indigo-600 text-white"
                    : "bg-slate-900 border border-slate-800 text-slate-400 hover:text-slate-200"
                }`}
              >
                {st}
              </button>
            ))}
          </div>

          <div
            data-testid="pr-list"
            className="space-y-3 flex-1 min-h-0 max-h-160 lg:max-h-none overflow-y-auto pr-1"
          >
            {proposals.map((pr) => {
              const isSelected = selectedPR?.id === pr.id
              return (
                <div
                  key={pr.id}
                  onClick={() => handleSelectPR(pr)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      handleSelectPR(pr)
                    }
                  }}
                  className={`p-4 rounded-xl border text-left cursor-pointer transition-all ${
                    isSelected
                      ? "bg-indigo-950/30 border-indigo-500/50 shadow-lg shadow-indigo-950/40"
                      : "bg-slate-900 border-slate-800 hover:border-slate-700 hover:bg-slate-800/40"
                  }`}
                >
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-xs font-bold text-white flex items-center gap-1.5">
                      <GitPullRequest className="h-3.5 w-3.5 text-indigo-400" />
                      PR #{pr.id}
                    </span>
                    <span
                      className={`text-[10px] font-mono px-2 py-0.5 rounded-full font-medium ${
                        pr.status === "merged"
                          ? "bg-purple-500/10 text-purple-400 border border-purple-500/20"
                          : pr.status === "closed"
                            ? "bg-amber-500/10 text-amber-400 border border-amber-500/20"
                            : "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                      }`}
                    >
                      {pr.status}
                    </span>
                  </div>

                  <p className="text-xs font-medium text-slate-200 line-clamp-2 mb-2">
                    {pr.title}
                  </p>

                  <div className="flex items-center justify-between text-[10px] font-mono text-slate-500">
                    <span>{pr.tenant}</span>
                    <span>{pr.createdAt}</span>
                  </div>
                </div>
              )
            })}
            {proposals.length === 0 && (
              <div className="p-4 rounded-xl bg-slate-900 border border-slate-800 text-xs text-slate-500 text-center">
                No open proposals found — create a proposal from the Catalog.
              </div>
            )}
          </div>
        </div>

        {/* Right Preview Inspector Tree & Details */}
        <div className="lg:col-span-8 flex flex-col lg:h-full min-h-0">
          {selectedPR ? (
            <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-6 lg:h-full min-h-0 overflow-y-auto">
              {/* Selected PR header */}
              <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 border-b border-slate-800 pb-4">
                <div>
                  <div className="flex items-center space-x-2">
                    <h3 className="text-lg font-bold text-white">
                      PR #{selectedPR.id}: {selectedPR.title}
                    </h3>
                  </div>
                  <p className="text-xs font-mono text-slate-400 mt-1">
                    Branch:{" "}
                    <span className="text-indigo-400">{selectedPR.branch}</span>{" "}
                    | Author: {selectedPR.author}
                  </p>
                </div>

                <div className="flex items-center space-x-2">
                  {selectedPR.prUrl && (
                    <div className="relative group inline-flex">
                      <a
                        href={selectedPR.prUrl}
                        target="_blank"
                        rel="noreferrer"
                        aria-label="Forge Link"
                        className="inline-flex items-center justify-center p-2 bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 rounded-lg transition-colors"
                      >
                        <ExternalLink className="h-4 w-4" />
                      </a>
                      <span
                        role="tooltip"
                        className="pointer-events-none absolute top-full left-1/2 -translate-x-1/2 mt-2 px-2 py-1 rounded-md bg-slate-950 border border-slate-700 text-[11px] font-medium text-slate-200 whitespace-nowrap opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 transition-opacity shadow-lg z-20"
                      >
                        Forge Link
                      </span>
                    </div>
                  )}

                  {selectedPR.status === "closed"
                    ? (() => {
                        const reopenLabel = reopening
                          ? "Reopening..."
                          : "Reopen PR"
                        return (
                          <div className="relative group inline-flex">
                            <button
                              onClick={handleReopenPR}
                              disabled={merging || rejecting || reopening}
                              aria-label={reopenLabel}
                              className="inline-flex items-center justify-center p-2 bg-emerald-600/20 enabled:hover:bg-emerald-600/30 text-emerald-300 border border-emerald-500/30 rounded-lg transition-colors disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer"
                            >
                              {reopening ? (
                                <Loader2 className="h-4 w-4 animate-spin" />
                              ) : (
                                <RotateCcw className="h-4 w-4" />
                              )}
                            </button>
                            <span
                              role="tooltip"
                              className="pointer-events-none absolute top-full left-1/2 -translate-x-1/2 mt-2 px-2 py-1 rounded-md bg-slate-950 border border-slate-700 text-[11px] font-medium text-slate-200 whitespace-nowrap opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 transition-opacity shadow-lg z-20"
                            >
                              {reopenLabel}
                            </span>
                          </div>
                        )
                      })()
                    : (() => {
                        const rejectLabel = rejecting
                          ? "Rejecting..."
                          : "Reject PR"
                        return (
                          <div className="relative group inline-flex">
                            <button
                              onClick={handleRejectPR}
                              disabled={
                                merging ||
                                rejecting ||
                                reopening ||
                                selectedPR.status !== "open"
                              }
                              aria-label={rejectLabel}
                              className="inline-flex items-center justify-center p-2 bg-rose-600/20 enabled:hover:bg-rose-600/30 text-rose-300 border border-rose-500/30 rounded-lg transition-colors disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer"
                            >
                              {rejecting ? (
                                <Loader2 className="h-4 w-4 animate-spin" />
                              ) : (
                                <XCircle className="h-4 w-4" />
                              )}
                            </button>
                            <span
                              role="tooltip"
                              className="pointer-events-none absolute top-full left-1/2 -translate-x-1/2 mt-2 px-2 py-1 rounded-md bg-slate-950 border border-slate-700 text-[11px] font-medium text-slate-200 whitespace-nowrap opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 transition-opacity shadow-lg z-20"
                            >
                              {rejectLabel}
                            </span>
                          </div>
                        )
                      })()}

                  {(() => {
                    const mergeLabel = merging
                      ? "Merging..."
                      : selectedPR.status === "merged"
                        ? "Merged"
                        : "Merge PR Proposal"
                    return (
                      <div className="relative group inline-flex">
                        <button
                          onClick={handleMergePR}
                          disabled={
                            merging ||
                            rejecting ||
                            reopening ||
                            selectedPR.status !== "open"
                          }
                          aria-label={mergeLabel}
                          className="inline-flex items-center justify-center p-2 bg-purple-600 enabled:hover:bg-purple-500 text-white rounded-lg transition-colors disabled:opacity-40 disabled:cursor-not-allowed disabled:shadow-none shadow-lg shadow-purple-600/20 cursor-pointer"
                        >
                          {merging ? (
                            <Loader2 className="h-4 w-4 animate-spin" />
                          ) : (
                            <GitMerge className="h-4 w-4" />
                          )}
                        </button>
                        <span
                          role="tooltip"
                          className="pointer-events-none absolute top-full right-0 mt-2 px-2 py-1 rounded-md bg-slate-950 border border-slate-700 text-[11px] font-medium text-slate-200 whitespace-nowrap opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 transition-opacity shadow-lg z-20"
                        >
                          {mergeLabel}
                        </span>
                      </div>
                    )
                  })()}
                </div>
              </div>

              {mergeSuccess && (
                <div
                  role="alert"
                  className="p-4 bg-purple-950/40 border border-purple-500/30 rounded-xl text-purple-300 text-xs flex items-center space-x-2"
                >
                  <CheckCircle2 className="h-4 w-4 text-purple-400 shrink-0" />
                  <span>{mergeSuccess}</span>
                </div>
              )}

              {error && (
                <div
                  role="alert"
                  className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-center space-x-2"
                >
                  <AlertTriangle className="h-4 w-4 text-red-400 shrink-0" />
                  <span>{error}</span>
                </div>
              )}

              {/* Policy Check Status Banner */}
              <div className="flex items-center justify-between p-4 bg-slate-950 border border-slate-800 rounded-xl">
                <div className="flex items-center space-x-3">
                  <div className="p-2 bg-emerald-500/10 text-emerald-400 rounded-lg border border-emerald-500/20">
                    <ShieldCheck className="h-5 w-5" />
                  </div>
                  <div>
                    <p className="text-xs font-semibold text-white">
                      Kyverno &amp; PKO Policy Guardrails
                    </p>
                    <p className="text-[11px] text-slate-400">
                      Disallow-privileged, restrict-pulumi-providers &amp;
                      restricted PSS passed
                    </p>
                  </div>
                </div>
                <span className="text-xs px-2.5 py-0.5 rounded-full bg-emerald-500/10 text-emerald-400 border border-emerald-500/20 font-medium">
                  {preview?.policyStatus || "passed"}
                </span>
              </div>

              {/* Summary Counts Bar */}
              {preview?.summary && (
                <div className="grid grid-cols-2 sm:grid-cols-5 gap-3 text-center">
                  <div className="bg-slate-950 border border-slate-800 p-3 rounded-xl">
                    <p className="text-[10px] text-slate-500 uppercase font-mono">
                      Create
                    </p>
                    <p className="text-lg font-bold text-emerald-400 mt-0.5">
                      +{preview.summary.create}
                    </p>
                  </div>
                  <div className="bg-slate-950 border border-slate-800 p-3 rounded-xl">
                    <p className="text-[10px] text-slate-500 uppercase font-mono">
                      Update
                    </p>
                    <p className="text-lg font-bold text-blue-400 mt-0.5">
                      ~{preview.summary.update}
                    </p>
                  </div>
                  <div className="bg-slate-950 border border-slate-800 p-3 rounded-xl">
                    <p className="text-[10px] text-slate-500 uppercase font-mono">
                      Delete
                    </p>
                    <p className="text-lg font-bold text-red-400 mt-0.5">
                      -{preview.summary.delete}
                    </p>
                  </div>
                  <div className="bg-slate-950 border border-slate-800 p-3 rounded-xl">
                    <p className="text-[10px] text-slate-500 uppercase font-mono">
                      Replace
                    </p>
                    <p className="text-lg font-bold text-amber-400 mt-0.5">
                      !{preview.summary.replace}
                    </p>
                  </div>
                  <div className="bg-slate-950 border border-slate-800 p-3 rounded-xl">
                    <p className="text-[10px] text-slate-500 uppercase font-mono">
                      Unchanged
                    </p>
                    <p className="text-lg font-bold text-slate-400 mt-0.5">
                      ={preview.summary.same}
                    </p>
                  </div>
                </div>
              )}

              {/* Git Diff (for GitOps config/tenant/catalog/provider & infra PRs) */}
              {preview?.gitDiff && (
                <div className="space-y-2">
                  <h4 className="text-xs font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-1.5">
                    <GitPullRequest className="h-4 w-4 text-emerald-400" />
                    Git Diff (vs. main)
                  </h4>
                  <GitDiffViewer diff={preview.gitDiff} />
                </div>
              )}

              {/* Diff Tree List */}
              <div className="space-y-3">
                <div className="flex items-center justify-between">
                  <h4 className="text-xs font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-2">
                    <Layers className="h-4 w-4 text-indigo-400" />
                    Resource Diff Tree
                  </h4>
                  {loadingPreview && (
                    <span className="text-xs text-indigo-400 flex items-center gap-1 font-mono">
                      <Loader2 className="h-3 w-3 animate-spin" /> Fetching PKO
                      diff...
                    </span>
                  )}
                </div>

                <div className="space-y-2 max-h-96 overflow-y-auto pr-1">
                  {(preview?.resources ?? []).map((res, idx) => (
                    <div
                      key={`${res.urn}-${idx}`}
                      className="bg-slate-950 border border-slate-800 rounded-xl p-4 space-y-2 hover:border-slate-700 transition-colors"
                    >
                      <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-2">
                        <div className="flex items-center space-x-2">
                          {renderOpBadge(res.op)}
                          <span className="text-xs font-bold text-slate-200">
                            {res.name}
                          </span>
                        </div>
                        <span className="text-[10px] font-mono text-slate-500 truncate max-w-xs">
                          {res.resource_type}
                        </span>
                      </div>

                      <p className="text-[11px] font-mono text-slate-500 truncate">
                        {res.urn}
                      </p>

                      {res.changed_fields.length > 0 && (
                        <div className="flex items-center flex-wrap gap-1.5 pt-1">
                          <span className="text-[10px] text-slate-500 font-mono">
                            Changed fields:
                          </span>
                          {(res.changed_fields ?? []).map((field) => (
                            <span
                              key={field}
                              className="text-[10px] font-mono px-2 py-0.5 rounded bg-slate-900 border border-slate-800 text-indigo-300"
                            >
                              {field}
                            </span>
                          ))}
                        </div>
                      )}
                    </div>
                  ))}

                  {preview?.resources.length === 0 && (
                    <div className="py-8 text-center text-xs text-slate-500">
                      No resource mutations found in this PKO v2 stack preview.
                    </div>
                  )}
                </div>
              </div>
            </div>
          ) : (
            <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 text-center text-slate-400 text-xs space-y-4">
              {error && (
                <div
                  role="alert"
                  className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-center space-x-2 text-left"
                >
                  <AlertTriangle className="h-4 w-4 text-red-400 shrink-0" />
                  <span>{error}</span>
                </div>
              )}
              <p>
                No proposal selected. Create a proposal from the Catalog to
                inspect PKO preview diffs.
              </p>
            </div>
          )}
        </div>
      </div>
    </div>
  )
}
