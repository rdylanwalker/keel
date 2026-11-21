import React, { useState } from "react"
import {
  ExternalLink,
  GitMerge,
  Loader2,
  RotateCcw,
  XCircle,
} from "lucide-react"

import { Proposal } from "../../types/api"
import { Button } from "../ui/button"
import { ConfirmDialog } from "../ui/confirm-dialog"

export type ProposalAction = "merge" | "reject" | "reopen"

export interface ProposalActionsProps {
  proposal: Proposal
  /** Mirrors the server's `require_pr_tenant_admin_access` (cluster admin or tenant admin). */
  canManage: boolean
  pending: ProposalAction | null
  onMerge: () => void
  onReject: () => void
  onReopen: () => void
}

/** Labeled Merge / Reject (confirmed) / Reopen controls for the selected PR (UI-12). */
export const ProposalActions: React.FC<ProposalActionsProps> = ({
  proposal,
  canManage,
  pending,
  onMerge,
  onReject,
  onReopen,
}) => {
  const [confirmReject, setConfirmReject] = useState(false)
  const busy = pending !== null
  const permissionHint = canManage
    ? undefined
    : `Requires cluster admin or tenant admin for "${proposal.tenant}"`

  return (
    <div className="flex flex-col items-start gap-1.5 @3xl/header:items-end">
      <div className="flex flex-wrap items-center gap-2">
        {proposal.prUrl && (
          <Button asChild variant="secondary">
            <a
              href={proposal.prUrl}
              target="_blank"
              rel="noreferrer"
              aria-label="Forge Link"
            >
              <ExternalLink className="h-3.5 w-3.5" />
              <span>Open in Forge</span>
            </a>
          </Button>
        )}

        {proposal.status === "closed" ? (
          <Button
            variant="secondary"
            onClick={onReopen}
            disabled={busy || !canManage}
            title={permissionHint}
            className="text-emerald-300"
          >
            {pending === "reopen" ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <RotateCcw className="h-3.5 w-3.5" />
            )}
            <span>{pending === "reopen" ? "Reopening..." : "Reopen PR"}</span>
          </Button>
        ) : (
          <Button
            variant="destructive-subtle"
            onClick={() => setConfirmReject(true)}
            disabled={busy || !canManage || proposal.status !== "open"}
            title={permissionHint}
          >
            {pending === "reject" ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <XCircle className="h-3.5 w-3.5" />
            )}
            <span>{pending === "reject" ? "Rejecting..." : "Reject PR"}</span>
          </Button>
        )}

        <Button
          onClick={onMerge}
          disabled={busy || !canManage || proposal.status !== "open"}
          title={permissionHint}
          className="bg-purple-600 hover:bg-purple-500 shadow-purple-600/20"
        >
          {pending === "merge" ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <GitMerge className="h-3.5 w-3.5" />
          )}
          <span>
            {pending === "merge"
              ? "Merging..."
              : proposal.status === "merged"
                ? "Merged"
                : "Merge PR"}
          </span>
        </Button>
      </div>
      {!canManage && proposal.status !== "merged" && (
        <p
          data-testid="pr-actions-permission-hint"
          className="text-[11px] text-slate-500"
        >
          {permissionHint}
        </p>
      )}

      <ConfirmDialog
        open={confirmReject}
        onOpenChange={setConfirmReject}
        title={`Reject PR #${proposal.id}?`}
        description={
          <>
            This closes the pull request without merging. The branch{" "}
            <code className="text-slate-300">{proposal.branch}</code> is kept
            and the PR can be reopened later; its ephemeral preview resources
            are garbage-collected.
          </>
        }
        confirmLabel="Reject PR"
        destructive
        onConfirm={() => {
          setConfirmReject(false)
          onReject()
        }}
      />
    </div>
  )
}
