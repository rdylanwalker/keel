import React, { useCallback, useEffect, useMemo, useState } from "react"
import { AlertTriangle, GitPullRequest, Info } from "lucide-react"

import {
  PLATFORM_SCOPE,
  PreviewReport,
  Proposal,
  ProposalRef,
  UnavailableRepository,
} from "../types/api"
import { GitDiffViewer } from "./GitDiffViewer"
import { PrNotificationBanner } from "./PrNotificationBanner"
import {
  getProposalListing,
  getProposalPreview,
  mergeProposal,
  proposalKey,
  rejectProposal,
  reopenProposal,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"
import { ApiError, errorMessage, visibleLoadError } from "../lib/errors"
import { useAuthz } from "../lib/authz-context"
import { demoFixtures } from "../lib/demo"
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
import { proposalRepositoryName } from "./preview/ProposalRepoLabel"
import { ProposalAction, ProposalActions } from "./preview/ProposalActions"
import { PageHeader } from "./ui/page-header"
import {
  isPreviewRunActive,
  PreviewRunStatus,
} from "./preview/PreviewRunStatus"
import { usePreviewRunRefresh } from "./preview/usePreviewRunRefresh"

export { PolicyStatusBanner } from "./preview/PolicyStatusBanner"

export interface PreviewInspectorViewProps {
  /** Lists one tenant's repository (`/api/v1/tenants/{tenant}/proposals`); all when unset. */
  selectedTenant?: string
  /** PR from the `#preview/<scope>/<id>` deep link. */
  selectedProposal?: ProposalRef
  /** Called when the user selects a PR so the parent can update `#preview/<scope>/<id>`. */
  onSelectProposal?: (ref: ProposalRef | null) => void
}

function createdAtMillis(p: Proposal): number {
  const t = Date.parse(p.createdAt)
  return Number.isNaN(t) ? 0 : t
}

/** Open first, then newest first (PR ids are only comparable within one repository). */
export function sortProposals(data: Proposal[]): Proposal[] {
  return [...data].sort((a, b) => {
    if (a.status === "open" && b.status !== "open") return -1
    if (a.status !== "open" && b.status === "open") return 1
    const byTime = createdAtMillis(b) - createdAtMillis(a)
    if (byTime !== 0) return byTime
    if (a.scope !== b.scope) return a.scope.localeCompare(b.scope)
    return Number(b.id) - Number(a.id)
  })
}

const sameRef = (p: Proposal, ref: ProposalRef | null | undefined) =>
  Boolean(ref) && p.scope === ref?.scope && p.id === ref?.id

const ACTION_COPY: Record<
  ProposalAction,
  {
    call: (ref: ProposalRef) => Promise<{ success: boolean; message?: string }>
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
  selectedProposal,
  onSelectProposal,
}) => {
  const demo = isDemoMode()
  const authz = useAuthz()
  const [proposals, setProposals] = useState<Proposal[]>(() =>
    demo ? demoFixtures().DEFAULT_PROPOSALS : []
  )
  const [unavailable, setUnavailable] = useState<UnavailableRepository[]>([])
  const [manualSelected, setManualSelected] = useState<ProposalRef | null>(null)
  const [previewState, setPreviewState] = useState<{
    key: string
    report: PreviewReport | null
  } | null>(() => {
    if (!demo) return null
    const { DEFAULT_PROPOSALS, DEFAULT_PREVIEW } = demoFixtures()
    return DEFAULT_PROPOSALS[0]
      ? { key: proposalKey(DEFAULT_PROPOSALS[0]), report: DEFAULT_PREVIEW }
      : null
  })
  const [loadingPRs, setLoadingPRs] = useState(!demo)
  const [searchQuery, setSearchQuery] = useState("")
  const [pending, setPending] = useState<ProposalAction | null>(null)
  const [actionSuccess, setActionSuccess] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  // Merge/reject/reopen failures; kept apart from load errors so a preview reload (e.g. after a
  // stale proposal was rebuilt on `main`) does not hide them.
  const [actionError, setActionError] = useState<string | null>(null)
  const [statusFilter, setStatusFilter] = useState<ProposalStatusFilter>("all")
  const [previewRefreshTick, setPreviewRefreshTick] = useState(0)

  // A deep link into another scope than the header tenant (e.g. a platform stub PR or a new
  // tenant's skeleton PR) lists every repository the caller may read, so the link resolves.
  const listingScope =
    selectedProposal &&
    selectedTenant &&
    selectedProposal.scope !== selectedTenant
      ? undefined
      : selectedTenant
  const listingKey = `${listingScope ?? "*"}|${statusFilter}`
  const [lastListingKey, setLastListingKey] = useState(listingKey)
  if (lastListingKey !== listingKey) {
    setLastListingKey(listingKey)
    if (!demo) setLoadingPRs(true)
  }

  useEffect(() => {
    // Demo mode never calls the live API (UI-9); fixtures are already in state.
    if (demo) return
    let cancelled = false
    getProposalListing(listingScope, statusFilter)
      .then((listing) => {
        if (cancelled) return
        setProposals(sortProposals(listing.proposals ?? []))
        setUnavailable(listing.unavailable ?? [])
      })
      .catch((err) => {
        if (cancelled) return
        setError(visibleLoadError(err, "Failed to load PR proposals"))
        setProposals([])
        setUnavailable([])
      })
      .finally(() => {
        if (!cancelled) setLoadingPRs(false)
      })
    return () => {
      cancelled = true
    }
  }, [demo, listingScope, statusFilter])

  // A new deep link (e.g. "Go to PR" elsewhere) supersedes an earlier manual click.
  const deepLinkKey = selectedProposal ? proposalKey(selectedProposal) : null
  const [lastDeepLink, setLastDeepLink] = useState(deepLinkKey)
  if (lastDeepLink !== deepLinkKey) {
    setLastDeepLink(deepLinkKey)
    setManualSelected(null)
  }

  // Selection precedence: explicit user click > deep link > first listed PR.
  const requested = manualSelected ?? selectedProposal ?? null
  const requestedMissing =
    !loadingPRs &&
    Boolean(selectedProposal) &&
    manualSelected === null &&
    !proposals.some((p) => sameRef(p, selectedProposal))
  const selectedPR = useMemo<Proposal | null>(
    () =>
      proposals.find((p) => sameRef(p, requested)) ??
      (requestedMissing || !requested ? (proposals[0] ?? null) : null),
    [proposals, requested, requestedMissing]
  )
  const selectedKey = selectedPR ? proposalKey(selectedPR) : undefined
  const selectedScope = selectedPR?.scope
  const selectedId = selectedPR?.id

  useEffect(() => {
    if (!selectedKey || !selectedScope || !selectedId || demo) return
    let cancelled = false
    getProposalPreview({ scope: selectedScope, id: selectedId })
      .then((report) => {
        if (cancelled) return
        setError(null)
        setPreviewState({ key: selectedKey, report })
      })
      .catch((err) => {
        if (cancelled) return
        setError(visibleLoadError(err, "Failed to load preview diff"))
        setPreviewState({ key: selectedKey, report: null })
      })
    return () => {
      cancelled = true
    }
  }, [selectedKey, selectedScope, selectedId, demo, previewRefreshTick])

  const preview =
    previewState && previewState.key === selectedKey
      ? previewState.report
      : null
  const loadingPreview =
    Boolean(selectedKey) && !demo && previewState?.key !== selectedKey

  const refreshPreview = useCallback(
    () => setPreviewRefreshTick((t) => t + 1),
    []
  )
  usePreviewRunRefresh(isPreviewRunActive(preview?.run), refreshPreview)

  const handleSelectPR = (pr: Proposal) => {
    const ref = { scope: pr.scope, id: pr.id }
    setManualSelected(ref)
    setActionSuccess(null)
    setActionError(null)
    onSelectProposal?.(ref)
  }

  const runAction = async (action: ProposalAction) => {
    if (!selectedPR) return
    const ref = { scope: selectedPR.scope, id: selectedPR.id }
    const id = ref.id
    const copy = ACTION_COPY[action]
    setPending(action)
    setActionSuccess(null)
    setError(null)
    setActionError(null)
    try {
      const result = await copy.call(ref)
      if (result.success) {
        setActionSuccess(result.message || copy.success(id))
        setProposals((prev) =>
          prev.map((p) =>
            sameRef(p, ref) ? { ...p, status: copy.nextStatus } : p
          )
        )
      } else {
        setActionError(result.message || copy.failure)
      }
    } catch (err) {
      setActionError(errorMessage(err, copy.failure))
      // A 409 merge may have rebuilt a stale branch on `main` and restarted its preview: show
      // the new diff and preview run.
      if (action === "merge" && err instanceof ApiError && err.status === 409) {
        refreshPreview()
      }
    } finally {
      setPending(null)
    }
  }

  const shownError = actionError ?? error
  const errorAlert = shownError && (
    <div
      role="alert"
      className="p-4 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-center space-x-2 text-left"
    >
      <AlertTriangle className="h-4 w-4 text-red-400 shrink-0" />
      <span className="break-words min-w-0">{shownError}</span>
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
            PR #{selectedProposal?.id} ({selectedProposal?.scope}) not found or
            not visible with the current filters and permissions.
          </span>
        </div>
      )}

      {unavailable.length > 0 && (
        <div
          data-testid="pr-unavailable-repositories"
          role="status"
          className="p-3 bg-slate-900 border border-amber-500/30 rounded-xl text-amber-300 text-xs space-y-1"
        >
          <p className="flex items-center gap-2 font-semibold">
            <AlertTriangle className="h-4 w-4 shrink-0" />
            Some repositories could not be listed; their pull requests may be
            missing or out of date.
          </p>
          <ul className="list-disc pl-8 space-y-0.5">
            {unavailable.map((u) => (
              <li key={`${u.scope}:${u.repository.label}`}>
                <code className="font-mono">
                  {u.repository.fullName ?? u.repository.label}
                </code>{" "}
                ({u.scope}): {u.reason}
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="grid grid-cols-1 lg:grid-cols-12 gap-6 lg:h-[calc(100vh-14rem)] lg:min-h-140">
        <ProposalList
          proposals={proposals}
          selectedKey={selectedKey}
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
                    label: "Repository",
                    value: proposalRepositoryName(selectedPR),
                    valueClassName: "text-slate-200",
                  },
                  {
                    label: "Tenant",
                    value:
                      selectedPR.scope === PLATFORM_SCOPE
                        ? "platform"
                        : selectedPR.scope,
                  },
                  {
                    label: "Branch",
                    value: selectedPR.branch,
                    valueClassName: "text-indigo-400",
                  },
                ]}
                actions={
                  <ProposalActions
                    proposal={selectedPR}
                    canManage={
                      selectedPR.scope === PLATFORM_SCOPE
                        ? authz.isAdmin
                        : authz.canAdminTenant(selectedPR.scope)
                    }
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
