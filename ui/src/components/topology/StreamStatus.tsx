import React from "react"

import { StreamState } from "./useTopologyData"

const STYLES: Record<StreamState, { dot: string; pill: string }> = {
  live: {
    dot: "bg-emerald-400",
    pill: "bg-emerald-500/10 text-emerald-400 border-emerald-500/20",
  },
  connecting: {
    dot: "bg-slate-400 animate-pulse",
    pill: "bg-slate-800 text-slate-300 border-slate-700",
  },
  polling: {
    dot: "bg-amber-400",
    pill: "bg-amber-500/10 text-amber-300 border-amber-500/20",
  },
  paused: {
    dot: "bg-amber-400",
    pill: "bg-amber-500/10 text-amber-400 border-amber-500/20",
  },
  demo: {
    dot: "bg-fuchsia-400",
    pill: "bg-fuchsia-500/10 text-fuchsia-300 border-fuchsia-500/30",
  },
}

export function streamLabel(
  state: StreamState,
  pollIntervalMs: number | null
): string {
  switch (state) {
    case "live":
      return "SSE Live Stream"
    case "connecting":
      return "Connecting…"
    case "paused":
      return pollIntervalMs
        ? `SSE Paused · polling ${Math.round(pollIntervalMs / 1000)}s`
        : "SSE Paused"
    case "polling":
      return `SSE disconnected · polling ${Math.round((pollIntervalMs ?? 0) / 1000)}s`
    case "demo":
      return "Demo data (no live stream)"
  }
}

/** Honest live-update indicator: SSE when connected, backoff polling otherwise (UI-17). */
export const StreamStatus: React.FC<{
  state: StreamState
  pollIntervalMs: number | null
}> = ({ state, pollIntervalMs }) => (
  <span
    role="status"
    data-testid="topology-stream-status"
    className={`flex items-center gap-1.5 px-3 py-1 rounded-full text-xs font-mono border ${STYLES[state].pill}`}
  >
    <span className={`w-2 h-2 rounded-full ${STYLES[state].dot}`} />
    <span>{streamLabel(state, pollIntervalMs)}</span>
  </span>
)
