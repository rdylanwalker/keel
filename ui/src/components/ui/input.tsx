import * as React from "react"

import { cn } from "@/lib/utils"

export const inputClassName =
  "w-full min-w-0 rounded-lg border border-slate-800 bg-slate-950 px-3 py-2 text-xs text-slate-200 font-mono placeholder:text-slate-600 transition-colors focus:outline-none focus:border-indigo-500 focus-visible:ring-2 focus-visible:ring-indigo-500/40 disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-red-500/70"

export const Input = React.forwardRef<
  HTMLInputElement,
  React.InputHTMLAttributes<HTMLInputElement>
>(({ className, type, ...props }, ref) => (
  <input
    ref={ref}
    type={type}
    data-slot="input"
    className={cn(inputClassName, className)}
    {...props}
  />
))
Input.displayName = "Input"
