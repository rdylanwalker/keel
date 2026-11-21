import React from "react"
import {
  Clock,
  Filter,
  Pause,
  Play,
  Radio,
  Terminal,
  Trash2,
} from "lucide-react"
import { CrdWatchEvent } from "../types/api"

export interface TelemetryStreamPanelProps {
  filteredEvents: CrdWatchEvent[]
  eventFilterKind: string
  onEventFilterKindChange: (kind: string) => void
  isStreamPaused: boolean
  onToggleStreamPaused: () => void
  onClearEvents: () => void
  onInspectStack: (namespace: string, name: string) => void
}

export const TelemetryStreamPanel: React.FC<TelemetryStreamPanelProps> = ({
  filteredEvents,
  eventFilterKind,
  onEventFilterKindChange,
  isStreamPaused,
  onToggleStreamPaused,
  onClearEvents,
  onInspectStack,
}) => {
  return (
    <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
      <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 border-b border-slate-800 pb-4">
        <div className="flex items-center space-x-3">
          <Radio className="h-5 w-5 text-indigo-400" />
          <div>
            <h3 className="text-base font-semibold text-white">
              Live Reconciler Event Stream
            </h3>
            <p className="text-xs text-slate-400">
              kube-rs CRD condition transitions for Flux &amp; PKO v2 Stacks
            </p>
          </div>
        </div>

        <div className="flex items-center space-x-3">
          <div className="flex items-center space-x-1 bg-slate-950 border border-slate-800 rounded-lg p-1">
            <Filter className="h-3.5 w-3.5 text-slate-500 ml-2" />
            <select
              aria-label="Filter events by CRD kind"
              value={eventFilterKind}
              onChange={(e) => onEventFilterKindChange(e.target.value)}
              style={{ colorScheme: "dark" }}
              className="bg-slate-950 border-0 text-xs text-slate-200 rounded focus:outline-none focus:ring-0 py-0.5 px-2"
            >
              <option value="all" className="bg-slate-950 text-slate-200">
                All CRDs
              </option>
              <option
                value="GitRepository"
                className="bg-slate-950 text-slate-200"
              >
                GitRepository
              </option>
              <option
                value="Kustomization"
                className="bg-slate-950 text-slate-200"
              >
                Kustomization
              </option>
              <option
                value="HelmRelease"
                className="bg-slate-950 text-slate-200"
              >
                HelmRelease
              </option>
              <option value="Stack" className="bg-slate-950 text-slate-200">
                Stack
              </option>
              <option value="Canary" className="bg-slate-950 text-slate-200">
                Canary
              </option>
            </select>
          </div>

          <button
            type="button"
            onClick={onToggleStreamPaused}
            className={`flex items-center space-x-1.5 px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors cursor-pointer ${
              isStreamPaused
                ? "bg-amber-500/10 text-amber-400 border-amber-500/30 hover:bg-amber-500/20"
                : "bg-slate-800 text-slate-300 border-slate-700 hover:bg-slate-700"
            }`}
          >
            {isStreamPaused ? (
              <>
                <Play className="h-3.5 w-3.5" />
                <span>Resume Stream</span>
              </>
            ) : (
              <>
                <Pause className="h-3.5 w-3.5" />
                <span>Pause Stream</span>
              </>
            )}
          </button>

          <button
            type="button"
            onClick={onClearEvents}
            className="p-1.5 text-slate-400 hover:text-red-400 bg-slate-950 hover:bg-slate-800 border border-slate-800 rounded-lg transition-colors cursor-pointer"
            title="Clear event logs"
          >
            <Trash2 className="h-4 w-4" />
          </button>
        </div>
      </div>

      <div className="bg-slate-950 border border-slate-800 rounded-xl p-4 font-mono text-xs space-y-2 max-h-96 overflow-y-auto">
        {filteredEvents.map((evt, idx) => (
          <div
            key={`${evt.name}-${idx}`}
            className="flex flex-col sm:flex-row sm:items-center justify-between p-2.5 rounded-lg bg-slate-900/60 border border-slate-800/80 hover:bg-slate-900 transition-colors gap-2"
          >
            <div className="flex items-start sm:items-center space-x-3 min-w-0 flex-1">
              <span className="text-[10px] text-slate-500 flex items-center gap-1 shrink-0 mt-0.5 sm:mt-0">
                <Clock className="h-3 w-3" />
                {evt.timestamp}
              </span>

              <span
                className={`text-[10px] px-2 py-0.5 rounded font-bold uppercase shrink-0 ${
                  evt.kind === "GitRepository"
                    ? "bg-purple-500/10 text-purple-400 border border-purple-500/20"
                    : evt.kind === "Kustomization"
                      ? "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                      : evt.kind === "HelmRelease"
                        ? "bg-indigo-500/10 text-indigo-400 border border-indigo-500/20"
                        : evt.kind === "Stack"
                          ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                          : "bg-amber-500/10 text-amber-400 border border-amber-500/20"
                }`}
              >
                {evt.kind}
              </span>

              <div className="min-w-0 flex-1">
                <span className="font-semibold text-slate-200">
                  {evt.namespace}/{evt.name}
                </span>
                <p className="text-slate-400 truncate mt-0.5">{evt.message}</p>
              </div>
            </div>

            <div className="flex items-center space-x-2 shrink-0 self-end sm:self-center">
              {evt.kind === "Stack" && (
                <button
                  type="button"
                  onClick={() => onInspectStack(evt.namespace, evt.name)}
                  className="inline-flex items-center space-x-1 text-[10px] px-2 py-0.5 rounded bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                >
                  <Terminal className="h-3 w-3" />
                  <span>Inspect</span>
                </button>
              )}
              {evt.revision && (
                <span
                  title={evt.revision}
                  className="text-[10px] px-1.5 py-0.5 rounded bg-slate-950 border border-slate-800 text-slate-400 max-w-44 truncate"
                >
                  {evt.revision}
                </span>
              )}
              <span
                className={`text-[10px] px-2 py-0.5 rounded font-bold ${
                  evt.status === "Ready" || evt.status === "Healthy"
                    ? "text-emerald-400 bg-emerald-500/10 border border-emerald-500/20"
                    : evt.status === "Progressing"
                      ? "text-blue-400 bg-blue-500/10 border border-blue-500/20"
                      : "text-red-400 bg-red-500/10 border border-red-500/20"
                }`}
              >
                {evt.status}
              </span>
            </div>
          </div>
        ))}

        {filteredEvents.length === 0 && (
          <div className="py-8 text-center text-slate-500 font-sans">
            No CRD events received yet or stream cleared.
          </div>
        )}
      </div>
    </div>
  )
}
