import * as React from "react"

import { cn } from "@/lib/utils"

export interface TruncatedTextProps extends React.HTMLAttributes<HTMLSpanElement> {
  /** Full value; rendered as the text and exposed via the native `title` tooltip. */
  text: string
  /** Defaults to the full `text`; pass `null` to suppress the tooltip. */
  title?: string
}

/**
 * Single-line text that ends in an ellipsis instead of wrapping (or breaking mid-token)
 * when it does not fit. Callers cap the width (e.g. `max-w-[14rem]`); the full value is
 * always available as a tooltip.
 */
export const TruncatedText = React.forwardRef<
  HTMLSpanElement,
  TruncatedTextProps
>(({ text, title, className, ...props }, ref) => (
  <span
    ref={ref}
    data-slot="truncated-text"
    title={title === undefined ? text : title || undefined}
    className={cn(
      "block min-w-0 max-w-full overflow-hidden text-ellipsis whitespace-nowrap",
      className
    )}
    {...props}
  >
    {text}
  </span>
))
TruncatedText.displayName = "TruncatedText"
