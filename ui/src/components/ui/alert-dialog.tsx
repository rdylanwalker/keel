import * as React from "react"
import { AlertDialog as AlertDialogPrimitive } from "radix-ui"

import { cn } from "@/lib/utils"
import { buttonVariants } from "./button"
import { useCloseOnRouteChange } from "./dialog"

export function AlertDialog({
  open,
  onOpenChange,
  ...props
}: React.ComponentProps<typeof AlertDialogPrimitive.Root>) {
  useCloseOnRouteChange(Boolean(open), onOpenChange)
  return (
    <AlertDialogPrimitive.Root
      open={open}
      onOpenChange={onOpenChange}
      {...props}
    />
  )
}

export const AlertDialogContent = React.forwardRef<
  React.ComponentRef<typeof AlertDialogPrimitive.Content>,
  React.ComponentPropsWithoutRef<typeof AlertDialogPrimitive.Content>
>(({ className, ...props }, ref) => (
  <AlertDialogPrimitive.Portal>
    <AlertDialogPrimitive.Overlay className="fixed inset-0 z-[60] bg-black/70 backdrop-blur-sm" />
    <AlertDialogPrimitive.Content
      ref={ref}
      data-slot="alert-dialog-content"
      className={cn(
        "fixed left-1/2 top-1/2 z-[60] grid w-[calc(100vw-1rem)] max-w-md -translate-x-1/2 -translate-y-1/2 gap-4 rounded-2xl border border-slate-800 bg-slate-900 p-5 text-slate-200 shadow-2xl shadow-black/60 focus:outline-none",
        className
      )}
      {...props}
    />
  </AlertDialogPrimitive.Portal>
))
AlertDialogContent.displayName = "AlertDialogContent"

export function AlertDialogHeader({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return <div className={cn("flex flex-col gap-1.5", className)} {...props} />
}

export function AlertDialogFooter({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "flex flex-col-reverse gap-2 sm:flex-row sm:justify-end",
        className
      )}
      {...props}
    />
  )
}

export const AlertDialogTitle = React.forwardRef<
  React.ComponentRef<typeof AlertDialogPrimitive.Title>,
  React.ComponentPropsWithoutRef<typeof AlertDialogPrimitive.Title>
>(({ className, ...props }, ref) => (
  <AlertDialogPrimitive.Title
    ref={ref}
    className={cn("text-sm font-bold text-white", className)}
    {...props}
  />
))
AlertDialogTitle.displayName = "AlertDialogTitle"

export const AlertDialogDescription = React.forwardRef<
  React.ComponentRef<typeof AlertDialogPrimitive.Description>,
  React.ComponentPropsWithoutRef<typeof AlertDialogPrimitive.Description>
>(({ className, ...props }, ref) => (
  <AlertDialogPrimitive.Description
    ref={ref}
    className={cn("text-xs leading-relaxed text-slate-400", className)}
    {...props}
  />
))
AlertDialogDescription.displayName = "AlertDialogDescription"

export const AlertDialogAction = React.forwardRef<
  React.ComponentRef<typeof AlertDialogPrimitive.Action>,
  React.ComponentPropsWithoutRef<typeof AlertDialogPrimitive.Action> & {
    variant?: "default" | "destructive"
  }
>(({ className, variant = "default", ...props }, ref) => (
  <AlertDialogPrimitive.Action
    ref={ref}
    className={cn(buttonVariants({ variant }), className)}
    {...props}
  />
))
AlertDialogAction.displayName = "AlertDialogAction"

export const AlertDialogCancel = React.forwardRef<
  React.ComponentRef<typeof AlertDialogPrimitive.Cancel>,
  React.ComponentPropsWithoutRef<typeof AlertDialogPrimitive.Cancel>
>(({ className, ...props }, ref) => (
  <AlertDialogPrimitive.Cancel
    ref={ref}
    className={cn(buttonVariants({ variant: "secondary" }), className)}
    {...props}
  />
))
AlertDialogCancel.displayName = "AlertDialogCancel"
