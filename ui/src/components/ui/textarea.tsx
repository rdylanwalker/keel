import * as React from "react"

import { cn } from "@/lib/utils"
import { inputClassName } from "./input"

export const Textarea = React.forwardRef<
  HTMLTextAreaElement,
  React.TextareaHTMLAttributes<HTMLTextAreaElement>
>(({ className, ...props }, ref) => (
  <textarea
    ref={ref}
    data-slot="textarea"
    className={cn(inputClassName, "min-h-20", className)}
    {...props}
  />
))
Textarea.displayName = "Textarea"
