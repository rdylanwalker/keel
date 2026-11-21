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
import { Button } from "./ui/button"
import { PageHeader } from "./ui/page-header"
import { TruncatedText } from "./ui/truncated-text"

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
      <PageHeader
        variant="section"
        inlineAt="2xl"
        className="border-b border-slate-800 pb-4"
        titleClassName="font-semibold"
        icon={<Radio className="h-5 w-5 text-indigo-400" />}
        title="Live Reconciler Event Stream"
        description={
          <>kube-rs CRD condition transitions for Flux &amp; PKO v2 Stacks</>
        }
        actions={
          <>
            <div className="flex h-8 items-center gap-1 rounded-lg border border-slate-800 bg-slate-950 px-2">
              <Filter className="h-3.5 w-3.5 shrink-0 text-slate-500" />
              <select
                aria-label="Filter events by CRD kind"
                value={eventFilterKind}
                onChange={(e) => onEventFilterKindChange(e.target.value)}
                style={{ colorScheme: "dark" }}
                className="bg-slate-950 border-0 text-xs text-slate-200 rounded focus:outline-none focus:ring-0 py-0.5 pl-1 pr-2 min-w-0"
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
            <Button
              variant="secondary"
              onClick={onToggleStreamPaused}
              className={
                isStreamPaused
                  ? "bg-amber-500/10 text-amber-400 border-amber-500/30 hover:bg-amber-500/20"
                  : "text-slate-300"
              }
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
            </Button>
            <Button
              variant="secondary"
              size="icon"
              onClick={onClearEvents}
              className="text-slate-400 hover:text-red-400"
              title="Clear event logs"
              aria-label="Clear event logs"
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </>
        }
      />

      <div className="@container/events bg-slate-950 border border-slate-800 rounded-xl p-3 sm:p-4 font-mono text-xs space-y-2 max-h-96 overflow-y-auto">
        {filteredEvents.map((evt, idx) => (
          <div
            key={`${evt.name}-${idx}`}
            className="flex flex-col @2xl/events:flex-row @2xl/events:items-center justify-between p-2.5 rounded-lg bg-slate-900/60 border border-slate-800/80 hover:bg-slate-900 transition-colors gap-2"
          >
            <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 min-w-0 flex-1 @md/events:flex-nowrap">
              <span className="text-[10px] text-slate-500 flex items-center gap-1 shrink-0 whitespace-nowrap">
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

              <div className="min-w-0 basis-full @md/events:basis-auto flex-1">
                <TruncatedText
                  text={`${evt.namespace}/${evt.name}`}
                  className="font-semibold text-slate-200"
                />
                <p className="text-slate-400 truncate mt-0.5">{evt.message}</p>
              </div>
            </div>

            <div className="flex flex-wrap items-center justify-end gap-2 min-w-0 self-end @2xl/events:shrink-0 @2xl/events:self-center">
              {evt.kind === "Stack" && (
                <button
                  type="button"
                  onClick={() => onInspectStack(evt.namespace, evt.name)}
                  className="inline-flex shrink-0 items-center gap-1 whitespace-nowrap text-[10px] px-2 py-0.5 rounded bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                >
                  <Terminal className="h-3 w-3" />
                  <span>Inspect</span>
                </button>
              )}
              {evt.revision && (
                <span
                  title={evt.revision}
                  className="min-w-0 text-[10px] px-1.5 py-0.5 rounded bg-slate-950 border border-slate-800 text-slate-400 max-w-44 truncate"
                >
                  {evt.revision}
                </span>
              )}
              <span
                className={`shrink-0 whitespace-nowrap text-[10px] px-2 py-0.5 rounded font-bold ${
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
