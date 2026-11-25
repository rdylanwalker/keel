import React from "react"
import { ArrowRight, CheckCircle2, GitPullRequest, X } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Proposal } from "@/types/api"
import { safeHttpUrl } from "@/lib/forgeLinks"
import { ProposalRepoLabel } from "@/components/preview/ProposalRepoLabel"
import { SecretNoticeList } from "@/components/catalog/SecretNoticeList"

/** Floating toast + inline success panel shown after a catalog action opened a PR. */
export const ProposalCreatedNotice: React.FC<{
  proposal: Proposal
  onDismiss: () => void
  onNavigateToPreview?: () => void
}> = ({ proposal, onDismiss, onNavigateToPreview }) => {
  const prUrl = safeHttpUrl(proposal.prUrl)
  return (
    <>
      <div
        aria-label="Proposal Floating Toast"
        role="status"
        className="fixed bottom-4 right-4 left-4 sm:left-auto sm:bottom-6 sm:right-6 z-40 sm:max-w-md bg-slate-900/95 backdrop-blur-md border border-emerald-500/40 shadow-2xl shadow-black/60 rounded-2xl p-4 text-slate-200 space-y-3"
      >
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-2.5 min-w-0">
            <CheckCircle2 className="h-5 w-5 text-emerald-400 shrink-0 mt-0.5" />
            <div className="min-w-0">
              <p className="text-xs font-bold text-white">
                PR #{proposal.id} Opened
              </p>
              <p className="text-[11px] text-slate-400 truncate font-mono mt-0.5">
                {proposal.branch}
              </p>
              <ProposalRepoLabel proposal={proposal} />
            </div>
          </div>
          <Button
            variant="ghost"
            size="icon-sm"
            onClick={onDismiss}
            aria-label="Dismiss notification"
          >
            <X className="h-3.5 w-3.5" />
          </Button>
        </div>
        <div className="flex items-center justify-end gap-2">
          {prUrl && (
            <Button asChild variant="secondary" size="sm">
              <a href={prUrl} target="_blank" rel="noreferrer">
                View on Forge
              </a>
            </Button>
          )}
          {onNavigateToPreview && (
            <Button
              size="sm"
              className="bg-purple-600 hover:bg-purple-500 shadow-purple-600/20"
              onClick={onNavigateToPreview}
            >
              <span>Go to PR</span>
              <ArrowRight className="h-3.5 w-3.5" />
            </Button>
          )}
        </div>
      </div>

      <div
        aria-label="Proposal Success Notification"
        className="bg-emerald-950/40 border border-emerald-500/30 p-4 sm:p-5 rounded-2xl text-emerald-300 space-y-3"
      >
        <div className="flex items-center gap-3">
          <div className="p-2 bg-emerald-500/20 text-emerald-400 rounded-lg">
            <CheckCircle2 className="h-6 w-6" />
          </div>
          <div className="min-w-0">
            <h3 className="text-sm font-bold text-white">
              Git PR Proposal Successfully Opened!
            </h3>
            <p className="text-xs text-emerald-300 break-words">
              PR #{proposal.id} created on branch{" "}
              <code className="font-mono bg-slate-900 px-1 py-0.5 rounded">
                {proposal.branch}
              </code>
            </p>
            <ProposalRepoLabel proposal={proposal} linked />
          </div>
        </div>
        <SecretNoticeList notices={proposal.secretNotices} />
        <div className="text-xs bg-slate-900/90 border border-slate-800 rounded-xl p-4 flex flex-col sm:flex-row items-start sm:items-center justify-between gap-3 text-slate-300 font-mono">
          <div className="min-w-0 break-words">
            <span className="text-slate-500">PR Title: </span>
            <span className="text-slate-100 font-semibold">
              {proposal.title}
            </span>
          </div>
          <div className="flex items-center gap-2 shrink-0 font-sans">
            {onNavigateToPreview && (
              <Button
                size="sm"
                className="bg-purple-600 hover:bg-purple-500 shadow-purple-600/20"
                onClick={onNavigateToPreview}
              >
                <span>Go to PR</span>
                <ArrowRight className="h-3.5 w-3.5" />
              </Button>
            )}
            {prUrl && (
              <Button asChild variant="success" size="sm">
                <a href={prUrl} target="_blank" rel="noreferrer">
                  <span>View PR on Forge</span>
                  <GitPullRequest className="h-3.5 w-3.5" />
                </a>
              </Button>
            )}
          </div>
        </div>
      </div>
    </>
  )
}
