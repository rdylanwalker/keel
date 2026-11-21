import * as React from "react"

import { cn } from "@/lib/utils"

export interface PageHeaderMetaItem {
  /** Stable key; defaults to the label. */
  key?: string
  label: string
  value: string
  /** Extra classes for the value (e.g. a highlight colour). */
  valueClassName?: string
}

/**
 * Container width (Tailwind container-query breakpoint) at which the actions group moves
 * from a full-width row below the title to the end of the title row.
 */
type InlineAt = "xl" | "2xl" | "3xl" | "4xl"

const ROW_CLASSES: Record<InlineAt, string> = {
  xl: "@xl/header:flex-row @xl/header:items-start @xl/header:justify-between",
  "2xl":
    "@2xl/header:flex-row @2xl/header:items-start @2xl/header:justify-between",
  "3xl":
    "@3xl/header:flex-row @3xl/header:items-start @3xl/header:justify-between",
  "4xl":
    "@4xl/header:flex-row @4xl/header:items-start @4xl/header:justify-between",
}

const ACTIONS_INLINE_CLASSES: Record<InlineAt, string> = {
  xl: "@xl/header:shrink-0 @xl/header:flex-nowrap @xl/header:justify-end",
  "2xl": "@2xl/header:shrink-0 @2xl/header:flex-nowrap @2xl/header:justify-end",
  "3xl": "@3xl/header:shrink-0 @3xl/header:flex-nowrap @3xl/header:justify-end",
  "4xl": "@4xl/header:shrink-0 @4xl/header:flex-nowrap @4xl/header:justify-end",
}

export interface PageHeaderProps extends Omit<
  React.HTMLAttributes<HTMLDivElement>,
  "title"
> {
  icon?: React.ReactNode
  title: React.ReactNode
  description?: React.ReactNode
  meta?: PageHeaderMetaItem[]
  actions?: React.ReactNode
  /** `page`: top-of-view card (h2); `section`: header inside an existing card (h3). */
  variant?: "page" | "section"
  /** Heading element; defaults to h2 for `page` and h3 for `section`. */
  as?: "h1" | "h2" | "h3" | "h4"
  titleClassName?: string
  /** Container width at which actions sit beside the title (default `3xl` ≈ 48rem). */
  inlineAt?: InlineAt
}

/**
 * Shared view / section header: a title block that wraps at word boundaries (metadata
 * items truncate individually with a tooltip) plus an actions group that always stays
 * together. Narrow containers stack the actions as a full-width row under the title;
 * wide containers align them to the end of the title row. Layout is driven by a
 * container query so headers inside split panes (e.g. the PR inspector) adapt too.
 */
export function PageHeader({
  icon,
  title,
  description,
  meta,
  actions,
  variant = "page",
  as,
  titleClassName,
  inlineAt = "3xl",
  className,
  ...props
}: PageHeaderProps) {
  const Heading = as ?? (variant === "page" ? "h2" : "h3")
  return (
    <div
      data-slot="page-header"
      className={cn(
        "@container/header",
        variant === "page" &&
          "rounded-2xl border border-slate-800 bg-slate-900 p-4 sm:p-6",
        className
      )}
      {...props}
    >
      <div className={cn("flex flex-col gap-3", ROW_CLASSES[inlineAt])}>
        <div className="min-w-0 flex-1">
          <Heading
            data-slot="page-header-title"
            className={cn(
              "flex items-start gap-2 font-bold leading-snug text-white text-pretty [overflow-wrap:break-word]",
              variant === "page" ? "text-lg sm:text-xl" : "text-base",
              titleClassName
            )}
          >
            {icon && (
              <span
                aria-hidden="true"
                className={cn(
                  "flex shrink-0 items-center [&_svg]:shrink-0",
                  variant === "page" ? "h-7" : "h-6"
                )}
              >
                {icon}
              </span>
            )}
            <span className="min-w-0">{title}</span>
          </Heading>
          {description && (
            <div className="mt-1 text-xs text-slate-400 [overflow-wrap:break-word]">
              {description}
            </div>
          )}
          {meta && meta.length > 0 && (
            <ul
              data-slot="page-header-meta"
              className="mt-1.5 flex min-w-0 flex-wrap gap-x-4 gap-y-1 font-mono text-xs text-slate-400"
            >
              {meta.map((m) => (
                <li
                  key={m.key ?? m.label}
                  className="flex min-w-0 max-w-full items-baseline gap-1"
                >
                  <span className="shrink-0 whitespace-nowrap">{`${m.label}: `}</span>
                  <span
                    title={m.value}
                    className={cn(
                      "min-w-0 overflow-hidden text-ellipsis whitespace-nowrap",
                      m.valueClassName
                    )}
                  >
                    {m.value}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
        {actions && (
          <div
            data-slot="page-header-actions"
            className={cn(
              "flex flex-wrap items-center gap-2",
              ACTIONS_INLINE_CLASSES[inlineAt]
            )}
          >
            {actions}
          </div>
        )}
      </div>
    </div>
  )
}
