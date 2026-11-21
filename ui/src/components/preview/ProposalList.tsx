import React from "react"
import { GitPullRequest, Loader2 } from "lucide-react"

import { Proposal } from "../../types/api"

export type ProposalStatusFilter = "all" | "open" | "merged" | "closed"

const STATUS_FILTERS: readonly ProposalStatusFilter[] = [
  "all",
  "open",
  "merged",
  "closed",
]

export function matchesProposalQuery(pr: Proposal, query: string): boolean {
  const q = query.trim().toLowerCase()
  if (!q) return true
  return [pr.title, pr.id, pr.branch, pr.author, pr.tenant].some((v) =>
    (v ?? "").toLowerCase().includes(q)
  )
}

const statusTone = (status: Proposal["status"]) =>
  status === "merged"
    ? "bg-purple-500/10 text-purple-400 border border-purple-500/20"
    : status === "closed"
      ? "bg-amber-500/10 text-amber-400 border border-amber-500/20"
      : "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"

export interface ProposalListProps {
  proposals: Proposal[]
  selectedId?: string
  loading: boolean
  statusFilter: ProposalStatusFilter
  onStatusFilterChange: (filter: ProposalStatusFilter) => void
  searchQuery: string
  onSearchQueryChange: (q: string) => void
  onSelect: (pr: Proposal) => void
}

export const ProposalList: React.FC<ProposalListProps> = ({
  proposals,
  selectedId,
  loading,
  statusFilter,
  onStatusFilterChange,
  searchQuery,
  onSearchQueryChange,
  onSelect,
}) => {
  const visible = proposals.filter((pr) =>
    matchesProposalQuery(pr, searchQuery)
  )
  return (
    <div className="lg:col-span-4 flex flex-col lg:h-full min-h-0 space-y-4">
      <div className="flex items-center justify-between shrink-0">
        <h3 className="text-xs font-semibold text-slate-400 uppercase tracking-wider">
          Pull Requests ({proposals.length})
        </h3>
        {loading && (
          <Loader2 className="h-3.5 w-3.5 text-indigo-400 animate-spin" />
        )}
      </div>

      <div
        className="flex flex-wrap items-center gap-1.5 shrink-0"
        role="group"
        aria-label="Filter pull requests by status"
      >
        {STATUS_FILTERS.map((st) => (
          <button
            key={st}
            type="button"
            aria-pressed={statusFilter === st}
            onClick={() => onStatusFilterChange(st)}
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

      <div className="mb-4">
        <input
          type="text"
          placeholder="Search pull requests..."
          aria-label="Search pull requests"
          data-testid="pr-search-input"
          value={searchQuery}
          onChange={(e) => onSearchQueryChange(e.target.value)}
          className="w-full px-3 py-2 bg-slate-900 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
        />
      </div>

      <div
        data-testid="pr-list"
        className="space-y-3 flex-1 min-h-0 max-h-160 lg:max-h-none overflow-y-auto p-0.5 pr-1.5"
      >
        {visible.map((pr) => {
          const isSelected = selectedId === pr.id
          return (
            <div
              key={pr.id}
              onClick={() => onSelect(pr)}
              role="button"
              tabIndex={0}
              aria-current={isSelected ? "true" : undefined}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault()
                  onSelect(pr)
                }
              }}
              className={`p-4 rounded-xl border text-left cursor-pointer transition-all focus:outline-none focus-visible:ring-2 focus-visible:ring-indigo-400 ${
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
                  className={`text-[10px] font-mono px-2 py-0.5 rounded-full font-medium ${statusTone(pr.status)}`}
                >
                  {pr.status}
                </span>
              </div>

              <p className="text-xs font-medium text-slate-200 line-clamp-2 mb-2">
                {pr.title}
              </p>

              <div className="flex items-center justify-between gap-2 text-[10px] font-mono text-slate-500">
                <span className="truncate">{pr.tenant}</span>
                <span className="shrink-0">{pr.createdAt}</span>
              </div>
            </div>
          )
        })}
        {proposals.length === 0 && (
          <div className="p-4 rounded-xl bg-slate-900 border border-slate-800 text-xs text-slate-500 text-center">
            No open proposals found — create a proposal from the Catalog.
          </div>
        )}
        {proposals.length > 0 && visible.length === 0 && (
          <div className="p-4 rounded-xl bg-slate-900 border border-slate-800 text-xs text-slate-500 text-center">
            No pull requests matching your search.
          </div>
        )}
      </div>
    </div>
  )
}
