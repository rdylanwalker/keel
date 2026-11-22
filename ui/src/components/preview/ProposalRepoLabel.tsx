import React from "react"
import { FolderGit2 } from "lucide-react"

import { Proposal } from "../../types/api"
import { safeHttpUrl } from "../../lib/forgeLinks"

/**
 * Human label of the repository a proposal lives in: the tenant repository `owner/name`, the
 * platform repository, or (Local Git provider) the shared development repository.
 */
export function proposalRepositoryName(proposal: Proposal): string {
  const repo = proposal.repository
  if (repo?.fullName) return repo.fullName
  if (repo?.label && repo.label !== "platform") return repo.label
  return "platform repository"
}

/** Compact repository line (optionally linking to the repository on the forge). */
export const ProposalRepoLabel: React.FC<{
  proposal: Proposal
  /** Render the repository as a link to its forge page when known. */
  linked?: boolean
  className?: string
}> = ({ proposal, linked = false, className = "" }) => {
  const name = proposalRepositoryName(proposal)
  const href = linked ? safeHttpUrl(proposal.repoUrl) : undefined
  return (
    <span
      data-testid="pr-repository"
      className={`inline-flex min-w-0 items-center gap-1 text-[10px] font-mono text-slate-400 ${className}`.trim()}
      title={`Repository: ${name}`}
    >
      <FolderGit2 className="h-3 w-3 shrink-0 text-slate-500" />
      {href ? (
        <a
          href={href}
          target="_blank"
          rel="noreferrer"
          className="truncate underline decoration-slate-600 hover:text-slate-200"
        >
          {name}
        </a>
      ) : (
        <span className="truncate">{name}</span>
      )}
    </span>
  )
}
