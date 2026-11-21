import React from "react"
import { ArrowRight, CheckCircle2, X } from "lucide-react"

import { Proposal } from "../../types/api"
import { Button } from "../ui/button"

export const ProposalToast: React.FC<{
  proposal: Proposal
  onDismiss: () => void
  onOpen: (id: string) => void
}> = ({ proposal, onDismiss, onOpen }) => (
  <div
    role="status"
    aria-label="Proposal Floating Toast"
    className="fixed bottom-4 right-4 left-4 sm:left-auto z-50 sm:max-w-md bg-slate-900/95 backdrop-blur-md border border-emerald-500/40 shadow-2xl shadow-black/60 rounded-2xl p-4 text-slate-200 space-y-3"
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
        </div>
      </div>
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss notification"
        className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer shrink-0"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </div>
    <div className="flex items-center justify-end gap-2">
      {proposal.prUrl && (
        <Button asChild variant="secondary" size="sm">
          <a href={proposal.prUrl} target="_blank" rel="noreferrer">
            View on Forge
          </a>
        </Button>
      )}
      <Button
        size="sm"
        className="bg-purple-600 hover:bg-purple-500 shadow-none"
        onClick={() => onOpen(proposal.id)}
      >
        <span>Go to PR</span>
        <ArrowRight className="h-3.5 w-3.5" />
      </Button>
    </div>
  </div>
)
