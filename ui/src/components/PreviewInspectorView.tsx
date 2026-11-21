import React, { useCallback, useEffect, useMemo, useState } from "react"
import { AlertTriangle, GitPullRequest, Info } from "lucide-react"

import { PreviewReport, Proposal } from "../types/api"
import { GitDiffViewer } from "./GitDiffViewer"
import { PrNotificationBanner } from "./PrNotificationBanner"
import {
  getProposalPreview,
  getProposals,
  mergeProposal,
  rejectProposal,
  reopenProposal,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"
import { errorMessage, visibleLoadError } from "../lib/errors"
import { useAuthz } from "../lib/authz-context"
import { DEFAULT_PREVIEW, DEFAULT_PROPOSALS } from "../lib/demoFixtures"
import {
  PolicyStatusBanner,
  shouldShowPolicyBanner,
} from "./preview/PolicyStatusBanner"
import { PreviewSourceBadge, previewTitle } from "./preview/PreviewSourceBadge"
import {
  PreviewSummaryBar,
  ResourceDiffTree,
} from "./preview/ResourceDiffSection"
import { ProposalList, ProposalStatusFilter } from "./preview/ProposalList"
import { ProposalAction, ProposalActions } from "./preview/ProposalActions"
import { PageHeader } from "./ui/page-header"
import {
  isPreviewRunActive,
  PreviewRunStatus,
} from "./preview/PreviewRunStatus"
import { usePreviewRunRefresh } from "./preview/usePreviewRunRefresh"

export { PolicyStatusBanner } from "./preview/PolicyStatusBanner"

export interface PreviewInspectorViewProps {
  selectedTenant?: string
  /** PR id from the `#preview/<id>` deep link. */
  selectedProposalId?: string
  /** Called when the user selects a PR so the parent can update `#preview/<id>`. */
  onSelectProposal?: (id: string | null) => void
}

function sortProposals(data: Proposal[]): Proposal[] {
  return [...data].sort((a, b) => {
    if (a.status === "open" && b.status !== "open") return -1
    if (a.status !== "open" && b.status === "open") return 1
    return Number(b.id) - Number(a.id)
  })
}

const ACTION_COPY: Record<
  ProposalAction,
  {
    call: (id: string) => Promise<{ success: boolean; message?: string }>
    nextStatus: Proposal["status"]
    success: (id: string) => string
    failure: string
  }
> = {
  merge: {
    call: mergeProposal,
    nextStatus: "merged",
    success: (id) => `PR #${id} successfully merged!`,
    failure: "Failed to merge PR proposal",
  },
  reject: {
    call: rejectProposal,
    nextStatus: "closed",
    success: (id) => `PR #${id} rejected and closed.`,
    failure: "Failed to reject PR proposal",
  },
  reopen: {
    call: reopenProposal,
    nextStatus: "open",
    success: (id) => `PR #${id} reopened.`,
    failure: "Failed to reopen PR proposal",
  },
}

export const PreviewInspectorView: React.FC<PreviewInspectorViewProps> = ({
  selectedTenant,
  selectedProposalId,
  onSelectProposal,
}) => {
  const demo = isDemoMode()
  const authz = useAuthz()
  const [proposals, setProposals] = useState<Proposal[]>(() =>
    demo ? DEFAULT_PROPOSALS : []
  )
  const [manualSelectedId, setManualSelectedId] = useState<string | null>(null)
  const [previewState, setPreviewState] = useState<{
    prId: string
    report: PreviewReport | null
  } | null>(() =>
    demo && DEFAULT_PROPOSALS[0]
      ? { prId: DEFAULT_PROPOSALS[0].id, report: DEFAULT_PREVIEW }
      : null
  )
  const [loadingPRs, setLoadingPRs] = useState(!demo)
  const [searchQuery, setSearchQuery] = useState("")
  const [pending, setPending] = useState<ProposalAction | null>(null)
  const [actionSuccess, setActionSuccess] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [statusFilter, setStatusFilter] = useState<ProposalStatusFilter>("all")
  const [previewRefreshTick, setPreviewRefreshTick] = useState(0)

  useEffect(() => {
    // Demo mode never calls the live API (UI-9); fixtures are already in state.
    if (demo) return
    let cancelled = false
    getProposals(selectedTenant, statusFilter)
      .then((data) => {
        if (cancelled) return
        setProposals(sortProposals(data ?? []))
      })
      .catch((err) => {
        if (cancelled) return
        setError(visibleLoadError(err, "Failed to load PR proposals"))
        setProposals([])
      })
      .finally(() => {
        if (!cancelled) setLoadingPRs(false)
      })
    return () => {
      cancelled = true
    }
  }, [demo, selectedTenant, statusFilter])

  // A new deep link (e.g. "Go to PR" elsewhere) supersedes an earlier manual click.
  const [lastDeepLink, setLastDeepLink] = useState(selectedProposalId)
  if (lastDeepLink !== selectedProposalId) {
    setLastDeepLink(selectedProposalId)
    setManualSelectedId(null)
  }

  // Selection precedence: explicit user click > deep link > first listed PR.
  const requestedId = manualSelectedId ?? selectedProposalId ?? null
  const requestedMissing =
    !loadingPRs &&
    Boolean(selectedProposalId) &&
    manualSelectedId === null &&
    !proposals.some((p) => p.id === selectedProposalId)
  const selectedPR = useMemo<Proposal | null>(
    () =>
      proposals.find((p) => p.id === requestedId) ??
      (requestedMissing || !requestedId ? (proposals[0] ?? null) : null),
    [proposals, requestedId, requestedMissing]
  )
  const selectedId = selectedPR?.id

  useEffect(() => {
    if (!selectedId || demo) return
    let cancelled = false
    getProposalPreview(selectedId)
      .then((report) => {
        if (cancelled) return
        setError(null)
        setPreviewState({ prId: selectedId, report })
      })
      .catch((err) => {
        if (cancelled) return
        setError(visibleLoadError(err, "Failed to load preview diff"))
        setPreviewState({ prId: selectedId, report: null })
      })
    return () => {
      cancelled = true
    }
  }, [selectedId, demo, previewRefreshTick])

  const preview =
    previewState && previewState.prId === selectedId
      ? previewState.report
      : null
  const loadingPreview =
    Boolean(selectedId) && !demo && previewState?.prId !== selectedId

  const refreshPreview = useCallback(
    () => setPreviewRefreshTick((t) => t + 1),
    []
  )
  usePreviewRunRefresh(isPreviewRunActive(preview?.run), refreshPreview)

  const handleSelectPR = (pr: Proposal) => {
    setManualSelectedId(pr.id)
    setActionSuccess(null)
    onSelectProposal?.(pr.id)
  }

  const runAction = async (action: ProposalAction) => {
    if (!selectedPR) return
    const id = selectedPR.id
    const copy = ACTION_COPY[action]
    setPending(action)
    setActionSuccess(null)
    setError(null)
    try {
      const result = await copy.call(id)
      if (result.success) {
        setActionSuccess(result.message || copy.success(id))
        setProposals((prev) =>
          prev.map((p) => (p.id === id ? { ...p, status: copy.nextStatus } : p))
        )
      } else {
        setError(result.message || copy.failure)
      }
    } catch (err) {
      setError(errorMessage(err, copy.failure))
    } finally {
      setPending(null)
    }
  }

  const errorAlert = error && (
    <div
      role="alert"
      className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-center space-x-2 text-left"
    >
      <AlertTriangle className="h-4 w-4 text-red-400 shrink-0" />
      <span className="break-words min-w-0">{error}</span>
    </div>
  )

  return (
    <div className="space-y-6">
      <PageHeader
        icon={<GitPullRequest className="h-6 w-6 text-indigo-400" />}
        title="PR & Preview Inspector"
        description={
          <>
            Review proposed changes (
            <code className="text-slate-300">+ Create</code>,{" "}
            <code className="text-slate-300">~ Update</code>,{" "}
            <code className="text-slate-300">- Delete</code>,{" "}
            <code className="text-slate-300">! Replace</code>), inspect code
            diffs, and merge or reject pull requests
          </>
        }
      />

      {requestedMissing && (
        <div
          data-testid="pr-deeplink-missing"
          role="status"
          className="p-3 bg-slate-900 border border-amber-500/30 rounded-xl text-amber-300 text-xs flex items-center gap-2"
        >
          <Info className="h-4 w-4 shrink-0" />
          <span>
            PR #{selectedProposalId} not found or not visible with the current
            filters and permissions.
          </span>
        </div>
      )}

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-6 lg:h-[calc(100vh-14rem)] lg:min-h-140">
        <ProposalList
          proposals={proposals}
          selectedId={selectedId}
          loading={loadingPRs}
          statusFilter={statusFilter}
          onStatusFilterChange={(st) => {
            setStatusFilter(st)
            setLoadingPRs(!demo)
          }}
          searchQuery={searchQuery}
          onSearchQueryChange={setSearchQuery}
          onSelect={handleSelectPR}
        />

        <div className="lg:col-span-8 flex flex-col lg:h-full min-h-0">
          {selectedPR ? (
            <div className="bg-slate-900 border border-slate-800 rounded-2xl p-4 sm:p-6 space-y-6 lg:h-full min-h-0 overflow-y-auto">
              <PageHeader
                variant="section"
                className="border-b border-slate-800 pb-4"
                titleClassName="text-lg"
                title={`PR #${selectedPR.id}: ${selectedPR.title}`}
                meta={[
                  {
                    label: "Branch",
                    value: selectedPR.branch,
                    valueClassName: "text-indigo-400",
                  },
                  { label: "Author", value: selectedPR.author },
                ]}
                actions={
                  <ProposalActions
                    proposal={selectedPR}
                    canManage={authz.canAdminTenant(selectedPR.tenant)}
                    pending={pending}
                    onMerge={() => void runAction("merge")}
                    onReject={() => void runAction("reject")}
                    onReopen={() => void runAction("reopen")}
                  />
                }
              />

              {errorAlert}

              <div className="flex flex-wrap items-center gap-2">
                <h4
                  data-testid="preview-section-title"
                  className="text-sm font-semibold text-white"
                >
                  {previewTitle(preview?.source)}
                </h4>
                {preview && <PreviewSourceBadge source={preview.source} />}
              </div>

              <PreviewRunStatus run={preview?.run} />

              {preview?.source === "synthesized" && (
                <div
                  data-testid="simulated-preview-badge"
                  className="flex items-center space-x-2 p-3 bg-amber-950/30 border border-amber-500/30 rounded-xl text-amber-300 text-xs"
                >
                  <AlertTriangle className="h-4 w-4 text-amber-400 shrink-0" />
                  <span>
                    <span className="font-semibold">
                      Simulated diff — PKO preview not executed.
                    </span>{" "}
                    Resources below were synthesized from the proposed
                    manifests, not from a live <code>pulumi preview</code>.
                  </span>
                </div>
              )}

              {shouldShowPolicyBanner(preview) && (
                <PolicyStatusBanner status={preview?.policyStatus} />
              )}

              {preview?.summary && (
                <PreviewSummaryBar summary={preview.summary} />
              )}

              {preview?.gitDiff && (
                <div className="space-y-2">
                  <h4 className="text-xs font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-1.5">
                    <GitPullRequest className="h-4 w-4 text-emerald-400" />
                    Git Diff (vs. main)
                  </h4>
                  <GitDiffViewer diff={preview.gitDiff} />
                </div>
              )}

              <ResourceDiffTree
                resources={preview?.resources}
                loading={loadingPreview}
                hasPreview={Boolean(preview)}
              />
            </div>
          ) : (
            <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 text-center text-slate-400 text-xs space-y-4">
              {errorAlert}
              <p>
                No proposal selected. Create a proposal from the Catalog to
                inspect its preview diff.
              </p>
            </div>
          )}
        </div>
      </div>

      {actionSuccess && (
        <PrNotificationBanner
          message={actionSuccess}
          onDismiss={() => setActionSuccess(null)}
          variant="snackbar"
        />
      )}
    </div>
  )
}
