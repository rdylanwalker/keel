import * as React from "react"
import { ChevronDown } from "lucide-react"

import { cn } from "@/lib/utils"
import { inputClassName } from "./input"

/**
 * Shadcn UI `NativeSelect`: a styled native `<select>` (keeps platform a11y, mobile pickers,
 * and form semantics) used for rjsf enum widgets and simple pickers.
 */
export const NativeSelect = React.forwardRef<
  HTMLSelectElement,
  React.SelectHTMLAttributes<HTMLSelectElement>
>(({ className, children, ...props }, ref) => (
  <div className="relative w-full" data-slot="native-select-wrapper">
    <select
      ref={ref}
      data-slot="native-select"
      className={cn(
        inputClassName,
        "appearance-none pr-8 cursor-pointer",
        className
      )}
      {...props}
    >
      {children}
    </select>
    <ChevronDown
      aria-hidden="true"
      className="pointer-events-none absolute right-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-slate-500"
    />
  </div>
))
NativeSelect.displayName = "NativeSelect"
