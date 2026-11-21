import * as React from "react"
import { Dialog as DialogPrimitive } from "radix-ui"
import { Maximize2, Minimize2, X } from "lucide-react"

import { cn } from "@/lib/utils"

/**
 * Shadcn UI Dialog built on Radix primitives: Escape-to-close, focus trap, focus return
 * to the trigger, `aria-modal`, and scroll locking. Additionally closes itself when the
 * hash route changes so an inspector never lingers over a different page (UI-5).
 */
export function Dialog({
  open,
  onOpenChange,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Root>) {
  useCloseOnRouteChange(Boolean(open), onOpenChange)
  return (
    <DialogPrimitive.Root
      data-slot="dialog"
      open={open}
      onOpenChange={onOpenChange}
      {...props}
    />
  )
}

// eslint-disable-next-line react-refresh/only-export-components
export function useCloseOnRouteChange(
  open: boolean,
  onOpenChange?: (open: boolean) => void
) {
  const handlerRef = React.useRef(onOpenChange)
  React.useEffect(() => {
    handlerRef.current = onOpenChange
  }, [onOpenChange])
  React.useEffect(() => {
    if (!open) return
    const close = () => handlerRef.current?.(false)
    window.addEventListener("hashchange", close)
    window.addEventListener("popstate", close)
    return () => {
      window.removeEventListener("hashchange", close)
      window.removeEventListener("popstate", close)
    }
  }, [open])
}

export const DialogTrigger = DialogPrimitive.Trigger
export const DialogPortal = DialogPrimitive.Portal
export const DialogClose = DialogPrimitive.Close

export const DialogOverlay = React.forwardRef<
  React.ComponentRef<typeof DialogPrimitive.Overlay>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Overlay>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Overlay
    ref={ref}
    data-slot="dialog-overlay"
    className={cn("fixed inset-0 z-50 bg-black/70 backdrop-blur-sm", className)}
    {...props}
  />
))
DialogOverlay.displayName = "DialogOverlay"

const sizeClasses = {
  sm: "sm:max-w-md",
  md: "sm:max-w-lg",
  lg: "sm:max-w-2xl",
  xl: "sm:max-w-4xl",
  full: "sm:max-w-6xl",
} as const

export interface DialogContentProps extends React.ComponentPropsWithoutRef<
  typeof DialogPrimitive.Content
> {
  size?: keyof typeof sizeClasses
  hideClose?: boolean
  closeLabel?: string
  /**
   * Extra header controls (e.g. a {@link DialogFullscreenToggle}) rendered in the same
   * top-right flex group as the close button so they never overlap; the header reserves
   * room for the whole group.
   */
  controls?: React.ReactNode
}

export const DialogContent = React.forwardRef<
  React.ComponentRef<typeof DialogPrimitive.Content>,
  DialogContentProps
>(
  (
    {
      className,
      children,
      size = "md",
      hideClose = false,
      closeLabel = "Close",
      controls,
      ...props
    },
    ref
  ) => {
    const controlCount =
      React.Children.toArray(controls).length + (hideClose ? 0 : 1)
    return (
      <DialogPortal>
        <DialogOverlay />
        <DialogPrimitive.Content
          ref={ref}
          data-slot="dialog-content"
          data-controls={controlCount}
          className={cn(
            "group/dialog fixed left-1/2 top-1/2 z-50 flex w-[calc(100vw-1rem)] max-h-[calc(100dvh-1rem)] -translate-x-1/2 -translate-y-1/2 flex-col overflow-hidden rounded-2xl border border-slate-800 bg-slate-900 text-slate-200 shadow-2xl shadow-black/60 focus:outline-none",
            sizeClasses[size],
            className
          )}
          {...props}
        >
          {children}
          {controlCount > 0 && (
            <div
              data-slot="dialog-controls"
              className="absolute right-3 top-3 z-10 flex items-center gap-1"
            >
              {controls}
              {!hideClose && (
                <DialogPrimitive.Close
                  aria-label={closeLabel}
                  className="inline-flex h-7 w-7 items-center justify-center rounded-lg text-slate-400 transition-colors hover:bg-slate-800 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-indigo-400 cursor-pointer"
                >
                  <X className="h-4 w-4" />
                </DialogPrimitive.Close>
              )}
            </div>
          )}
        </DialogPrimitive.Content>
      </DialogPortal>
    )
  }
)
DialogContent.displayName = "DialogContent"

export function DialogHeader({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      data-slot="dialog-header"
      className={cn(
        "flex flex-col gap-1 border-b border-slate-800 px-5 py-4 sm:px-6",
        // Reserve room for the top-right control group (close + optional extras).
        "group-data-[controls=1]/dialog:pr-12 sm:group-data-[controls=1]/dialog:pr-12",
        "group-data-[controls=2]/dialog:pr-20 sm:group-data-[controls=2]/dialog:pr-20",
        "group-data-[controls=3]/dialog:pr-28 sm:group-data-[controls=3]/dialog:pr-28",
        className
      )}
      {...props}
    />
  )
}

export function DialogBody({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      data-slot="dialog-body"
      className={cn(
        "min-h-0 flex-1 overflow-y-auto px-5 py-4 sm:px-6",
        className
      )}
      {...props}
    />
  )
}

export function DialogFooter({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      data-slot="dialog-footer"
      className={cn(
        "flex flex-col-reverse gap-2 border-t border-slate-800 bg-slate-950/40 px-5 py-3 sm:flex-row sm:justify-end sm:px-6",
        className
      )}
      {...props}
    />
  )
}

export const DialogTitle = React.forwardRef<
  React.ComponentRef<typeof DialogPrimitive.Title>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Title>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Title
    ref={ref}
    data-slot="dialog-title"
    className={cn(
      "min-w-0 text-base font-bold leading-snug text-white [overflow-wrap:break-word]",
      className
    )}
    {...props}
  />
))
DialogTitle.displayName = "DialogTitle"

export const DialogDescription = React.forwardRef<
  React.ComponentRef<typeof DialogPrimitive.Description>,
  React.ComponentPropsWithoutRef<typeof DialogPrimitive.Description>
>(({ className, ...props }, ref) => (
  <DialogPrimitive.Description
    ref={ref}
    data-slot="dialog-description"
    className={cn(
      "text-xs text-slate-400 [overflow-wrap:break-word]",
      className
    )}
    {...props}
  />
))
DialogDescription.displayName = "DialogDescription"

export interface DialogFullscreenToggleProps {
  fullscreen: boolean
  onToggle: () => void
  enterLabel?: string
  exitLabel?: string
  className?: string
}

/** Fullscreen toggle sized to sit in {@link DialogContent}'s `controls` group. */
export function DialogFullscreenToggle({
  fullscreen,
  onToggle,
  enterLabel = "Enter full screen",
  exitLabel = "Exit full screen",
  className,
}: DialogFullscreenToggleProps) {
  const label = fullscreen ? exitLabel : enterLabel
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-label={label}
      title={label}
      data-slot="dialog-fullscreen-toggle"
      className={cn(
        "hidden h-7 w-7 items-center justify-center rounded-lg text-slate-400 transition-colors hover:bg-slate-800 hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-indigo-400 cursor-pointer sm:inline-flex",
        className
      )}
    >
      {fullscreen ? (
        <Minimize2 className="h-4 w-4" aria-hidden="true" />
      ) : (
        <Maximize2 className="h-4 w-4" aria-hidden="true" />
      )}
    </button>
  )
}
