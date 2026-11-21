import React from "react"
import { Gauge, GitBranch } from "lucide-react"
import { ReleaseSummary } from "../types/api"
import { StatusBadge } from "./StatusBadge"

export interface CanaryTrafficSectionProps {
  canaries: ReleaseSummary[]
}

export const CanaryTrafficSection: React.FC<CanaryTrafficSectionProps> = ({
  canaries,
}) => {
  if (canaries.length === 0) return null

  return (
    <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
      <div className="flex items-center justify-between border-b border-slate-800 pb-3">
        <div className="flex items-center space-x-3">
          <div className="p-2.5 bg-amber-500/10 text-amber-400 rounded-xl border border-amber-500/20">
            <Gauge className="h-5 w-5" />
          </div>
          <div>
            <h3 className="text-base font-semibold text-white">
              Flagger Canary Traffic Rollouts (Envoy Gateway)
            </h3>
            <p className="text-xs text-slate-400">
              Automated progressive traffic shifting with Prometheus error rate
              &amp; p99 latency evaluation
            </p>
          </div>
        </div>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {canaries.map((canary) => {
          const weight = canary.canaryWeight ?? 0
          const phase = canary.canaryPhase ?? ""
          const isPromoted =
            weight === 0 &&
            (phase === "Succeeded" ||
              phase === "Initialized" ||
              phase === "Promoting" ||
              phase === "Finalising")
          const primaryPct = isPromoted ? 100 : Math.max(0, 100 - weight)
          const primaryVer =
            canary.primaryVersion ||
            (isPromoted ? canary.version : undefined) ||
            "—"
          const canaryVer = canary.version || "—"
          const errRate = canary.errorRate
          const latency = canary.latencyMs

          return (
            <div
              key={canary.id || `${canary.namespace}-${canary.name}`}
              className="bg-slate-950 border border-slate-800 rounded-xl p-4 space-y-3"
            >
              <div className="flex items-center justify-between">
                <span className="text-xs font-bold text-white flex items-center gap-1.5">
                  <GitBranch className="h-3.5 w-3.5 text-amber-400" />
                  {canary.name}
                </span>
                <StatusBadge status={phase || canary.status} size="xs" />
              </div>

              <div className="space-y-1.5">
                <div className="flex items-center justify-between text-[11px]">
                  <span className="text-slate-400">
                    {isPromoted
                      ? "Traffic Split (Promoted)"
                      : "Canary Traffic Weight"}
                  </span>
                  <span
                    className={`font-mono font-bold ${
                      isPromoted
                        ? "text-emerald-400"
                        : phase === "Failed"
                          ? "text-red-400"
                          : "text-amber-400"
                    }`}
                  >
                    {isPromoted ? "100% Primary" : `${weight}%`}
                  </span>
                </div>
                <div className="w-full bg-slate-900 rounded-full h-2 overflow-hidden border border-slate-800 flex">
                  <div
                    className={`h-2 transition-all duration-500 ${
                      isPromoted
                        ? "bg-linear-to-r from-emerald-500 to-emerald-400"
                        : phase === "Failed"
                          ? "bg-red-500/80"
                          : "bg-indigo-500"
                    }`}
                    style={{ width: `${primaryPct}%` }}
                    title={`Primary ${primaryVer}: ${primaryPct}%`}
                  ></div>
                  {!isPromoted && weight > 0 && (
                    <div
                      className="bg-linear-to-r from-amber-400 to-emerald-400 h-2 transition-all duration-500"
                      style={{ width: `${weight}%` }}
                      title={`Canary ${canaryVer}: ${weight}%`}
                    ></div>
                  )}
                </div>
                <div className="flex items-center justify-between text-[10px] font-mono text-slate-400">
                  <span>
                    Primary:{" "}
                    <strong className="text-indigo-300">{primaryVer}</strong> (
                    {primaryPct}%)
                  </span>
                  {!isPromoted && (
                    <span>
                      Canary:{" "}
                      <strong className="text-amber-300">{canaryVer}</strong> (
                      {weight}%)
                    </span>
                  )}
                </div>
              </div>

              <div className="grid grid-cols-2 gap-2 pt-1 text-[10px] font-mono">
                <div className="bg-slate-900 p-2 rounded border border-slate-800">
                  <span className="text-slate-500 block">HTTP 5xx Rate:</span>
                  <span
                    className={`font-bold ${
                      errRate == null
                        ? "text-slate-500"
                        : errRate > 1
                          ? "text-red-400"
                          : "text-emerald-400"
                    }`}
                  >
                    {errRate != null ? `${errRate}%` : "—"}
                  </span>
                </div>
                <div className="bg-slate-900 p-2 rounded border border-slate-800">
                  <span className="text-slate-500 block">p99 Latency:</span>
                  <span
                    className={`font-bold ${
                      latency == null
                        ? "text-slate-500"
                        : latency > 500
                          ? "text-amber-400"
                          : "text-indigo-300"
                    }`}
                  >
                    {latency != null ? `${latency}ms` : "—"}
                  </span>
                </div>
              </div>
            </div>
          )
        })}
      </div>
    </div>
  )
}
