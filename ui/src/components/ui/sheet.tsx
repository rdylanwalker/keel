import * as React from "react"
import { Dialog as DialogPrimitive } from "radix-ui"
import { X } from "lucide-react"

import { cn } from "@/lib/utils"
import { Dialog } from "./dialog"

/** Shadcn UI Sheet (side drawer) on Radix Dialog; used for the mobile navigation drawer. */
export const Sheet = Dialog
export const SheetTitle = DialogPrimitive.Title

export const SheetContent = React.forwardRef<
  React.ComponentRef<typeof DialogPrimitive.Content>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Content>
>(({ className, children, ...props }, ref) => (
  <DialogPrimitive.Portal>
    <DialogPrimitive.Overlay className="fixed inset-0 z-50 bg-black/60" />
    <DialogPrimitive.Content
      ref={ref}
      data-slot="sheet-content"
      className={cn(
        "fixed inset-y-0 left-0 z-50 flex h-full w-72 max-w-[85vw] flex-col gap-2 border-r border-slate-800 bg-slate-900 p-4 shadow-2xl focus:outline-none",
        className
      )}
      {...props}
    >
      {children}
      <DialogPrimitive.Close
        aria-label="Close navigation"
        className="absolute right-3 top-3 rounded-lg p-1.5 text-slate-400 hover:bg-slate-800 hover:text-white cursor-pointer"
      >
        <X className="h-4 w-4" />
      </DialogPrimitive.Close>
    </DialogPrimitive.Content>
  </DialogPrimitive.Portal>
))
SheetContent.displayName = "SheetContent"
