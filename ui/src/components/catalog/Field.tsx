import React from "react"

import { Label } from "@/components/ui/label"
import { cn } from "@/lib/utils"

/** Label + control wrapper for hand-written catalog forms (Shadcn `Label`). */
export const Field: React.FC<{
  id: string
  label: React.ReactNode
  required?: boolean
  className?: string
  action?: React.ReactNode
  children: React.ReactNode
}> = ({ id, label, required, className, action, children }) => (
  <div className={cn("space-y-1", className)}>
    <div className="flex items-center justify-between gap-2">
      <Label htmlFor={id}>
        {label}
        {required && (
          <span className="ml-0.5 text-red-400" aria-hidden="true">
            {" "}
            *
          </span>
        )}
      </Label>
      {action}
    </div>
    {children}
  </div>
)

/** Checkbox + label row (Shadcn `Checkbox`). */
export const CheckboxRow: React.FC<{
  id: string
  label: React.ReactNode
  children: React.ReactNode
}> = ({ id, label, children }) => (
  <div className="flex items-center gap-2">
    {children}
    <Label htmlFor={id} className="cursor-pointer">
      {label}
    </Label>
  </div>
)
