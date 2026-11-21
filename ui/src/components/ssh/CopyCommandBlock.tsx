import React from "react"
import { Check, Copy } from "lucide-react"

import { Button } from "../ui/button"

export interface CopyCommandBlockProps {
  label: string
  command: string
  copied: boolean
  onCopy: () => void
  copyTestId: string
  commandTestId: string
  tone?: "emerald" | "indigo" | "amber"
}

const toneClass = {
  emerald: "text-emerald-400",
  indigo: "text-indigo-300",
  amber: "text-amber-300",
} as const

export const CopyCommandBlock: React.FC<CopyCommandBlockProps> = ({
  label,
  command,
  copied,
  onCopy,
  copyTestId,
  commandTestId,
  tone = "emerald",
}) => (
  <div className="bg-slate-950 border border-slate-800 rounded-xl p-3 space-y-1.5">
    <div className="flex items-center justify-between gap-2 text-xs text-slate-400">
      <span className="font-medium">{label}</span>
      <Button
        variant="secondary"
        size="sm"
        data-testid={copyTestId}
        onClick={onCopy}
        aria-label={`Copy: ${label}`}
      >
        {copied ? (
          <>
            <Check className="h-3 w-3 text-emerald-400" />
            <span className="text-emerald-400">Copied!</span>
          </>
        ) : (
          <>
            <Copy className="h-3 w-3" />
            <span>Copy</span>
          </>
        )}
      </Button>
    </div>
    <pre
      data-testid={commandTestId}
      className={`font-mono text-xs ${toneClass[tone]} bg-slate-900/90 rounded-lg p-2.5 overflow-x-auto select-all`}
    >
      {command}
    </pre>
  </div>
)
