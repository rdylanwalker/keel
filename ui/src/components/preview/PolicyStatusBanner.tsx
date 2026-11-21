import React from "react"
import { ShieldAlert, ShieldCheck, ShieldQuestion, ShieldX } from "lucide-react"

import { PreviewReport } from "../../types/api"

const POLICY_BANNER_STYLES: Record<
  "passed" | "failed" | "warning" | "unknown",
  {
    label: string
    description: string
    icon: React.ComponentType<{ className?: string }>
    tone: string
  }
> = {
  passed: {
    label: "passed",
    description: "All reported Kyverno & PKO policy checks passed",
    icon: ShieldCheck,
    tone: "bg-emerald-500/10 text-emerald-400 border-emerald-500/20",
  },
  failed: {
    label: "failed",
    description: "One or more Kyverno & PKO policy checks failed",
    icon: ShieldX,
    tone: "bg-red-500/10 text-red-400 border-red-500/20",
  },
  warning: {
    label: "warning",
    description: "Kyverno & PKO policy checks reported warnings",
    icon: ShieldAlert,
    tone: "bg-amber-500/10 text-amber-400 border-amber-500/20",
  },
  unknown: {
    label: "not evaluated",
    description: "Policy evaluation not available for this preview",
    icon: ShieldQuestion,
    tone: "bg-amber-500/10 text-amber-300 border-amber-500/20",
  },
}

export const PolicyStatusBanner: React.FC<{
  status?: PreviewReport["policyStatus"]
}> = ({ status }) => {
  const style = POLICY_BANNER_STYLES[status ?? "unknown"]
  const Icon = style.icon
  return (
    <div
      data-testid="policy-status-banner"
      data-status={status ?? "unknown"}
      className="flex items-center justify-between gap-3 p-4 bg-slate-950 border border-slate-800 rounded-xl"
    >
      <div className="flex items-center space-x-3 min-w-0">
        <div className={`p-2 rounded-lg border shrink-0 ${style.tone}`}>
          <Icon className="h-5 w-5" />
        </div>
        <div className="min-w-0">
          <p className="text-xs font-semibold text-white">
            Kyverno &amp; PKO Policy Guardrails
          </p>
          <p className="text-[11px] text-slate-400">{style.description}</p>
        </div>
      </div>
      <span
        className={`text-xs px-2.5 py-0.5 rounded-full border font-medium shrink-0 ${style.tone}`}
      >
        {style.label}
      </span>
    </div>
  )
}

/** Policy verdicts are only produced by a real PKO preview run that reported one. */
export function shouldShowPolicyBanner(
  preview:
    Pick<PreviewReport, "source" | "policyStatus" | "run"> | null | undefined
): boolean {
  return (
    preview?.source === "pko" &&
    preview.policyStatus !== undefined &&
    preview.run?.phase !== "notExecuted"
  )
}
