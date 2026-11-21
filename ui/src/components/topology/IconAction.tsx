import React from "react"

import { cn } from "../../lib/utils"

const TONES = {
  indigo:
    "bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border-indigo-500/30",
  "indigo-soft":
    "bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border-indigo-500/30",
  emerald:
    "bg-emerald-500/10 hover:bg-emerald-500/20 text-emerald-300 border-emerald-500/30",
  blue: "bg-blue-500/10 hover:bg-blue-500/20 text-blue-300 border-blue-500/30",
  red: "bg-red-500/10 hover:bg-red-500/20 text-red-300 border-red-500/30",
} as const

/** Compact icon-only row action with an accessible label and a hover title. */
export const IconAction: React.FC<{
  label: string
  title: string
  onClick: () => void
  disabled?: boolean
  tone: keyof typeof TONES
  testId?: string
  children: React.ReactNode
}> = ({ label, title, onClick, disabled, tone, testId, children }) => (
  <button
    type="button"
    aria-label={label}
    title={title}
    data-testid={testId}
    disabled={disabled}
    onClick={onClick}
    className={cn(
      "p-1 @md:p-1.5 rounded-md border transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-indigo-400",
      TONES[tone]
    )}
  >
    {children}
  </button>
)
