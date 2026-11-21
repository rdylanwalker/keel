import React from "react"
import { CheckCircle2, CircleSlash, Loader2, XCircle } from "lucide-react"

import { PreviewRun } from "../../types/api"

/** Renders the ephemeral PKO preview `Stack` run phase (pending/running/succeeded/failed/notExecuted). */
export const PreviewRunStatus: React.FC<{ run?: PreviewRun }> = ({ run }) => {
  if (!run) return null
  const base =
    "flex items-start gap-2 p-3 rounded-xl border text-xs break-words"
  switch (run.phase) {
    case "pending":
    case "running":
      return (
        <div
          data-testid="preview-run-status"
          data-phase={run.phase}
          role="status"
          className={`${base} bg-indigo-950/30 border-indigo-500/30 text-indigo-200`}
        >
          <Loader2 className="h-4 w-4 animate-spin shrink-0 text-indigo-400" />
          <span>
            <span className="font-semibold">
              {run.phase === "pending"
                ? "PKO preview pending"
                : "PKO preview running"}
            </span>
            {run.message ? ` — ${run.message}` : "…"}
          </span>
        </div>
      )
    case "failed":
      return (
        <div
          data-testid="preview-run-status"
          data-phase="failed"
          role="alert"
          className={`${base} bg-red-950/40 border-red-500/30 text-red-300`}
        >
          <XCircle className="h-4 w-4 shrink-0 text-red-400" />
          <span>
            <span className="font-semibold">PKO preview failed</span>
            {run.message ? `: ${run.message}` : ""}
          </span>
        </div>
      )
    case "notExecuted":
      return (
        <div
          data-testid="preview-run-status"
          data-phase="notExecuted"
          className={`${base} bg-slate-950 border-slate-800 text-slate-400`}
        >
          <CircleSlash className="h-4 w-4 shrink-0 text-slate-500" />
          <span>Not executed{run.message ? `: ${run.message}` : ""}</span>
        </div>
      )
    case "succeeded":
      return (
        <div
          data-testid="preview-run-status"
          data-phase="succeeded"
          className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-emerald-400"
        >
          <span className="inline-flex items-center gap-1.5">
            <CheckCircle2 className="h-3.5 w-3.5" />
            PKO preview succeeded
          </span>
          {run.stack && (
            <span className="font-mono text-slate-500">
              stack: <span className="text-slate-300">{run.stack}</span>
            </span>
          )}
          {run.commit && (
            <span className="font-mono text-slate-500">
              commit:{" "}
              <span className="text-slate-300">{run.commit.slice(0, 12)}</span>
            </span>
          )}
        </div>
      )
    default:
      return null
  }
}

export function isPreviewRunActive(run?: PreviewRun): boolean {
  return run?.phase === "pending" || run?.phase === "running"
}
