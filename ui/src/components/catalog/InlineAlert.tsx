import React from "react"
import { AlertCircle } from "lucide-react"

/** Inline error callout (`role="alert"`) used inside forms and dialogs. */
export const InlineAlert: React.FC<{
  children: React.ReactNode
  className?: string
}> = ({ children, className }) => (
  <div
    role="alert"
    className={`p-3 bg-red-950/40 border border-red-500/30 rounded-xl text-red-300 text-xs flex items-start gap-2 ${className ?? ""}`}
  >
    <AlertCircle className="h-4 w-4 text-red-400 shrink-0 mt-0.5" />
    <span className="min-w-0 break-words">{children}</span>
  </div>
)
