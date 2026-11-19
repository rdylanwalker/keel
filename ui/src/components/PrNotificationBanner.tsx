import React from "react"
import { CheckCircle2, X } from "lucide-react"

export interface PrNotificationBannerProps {
  message: string
  prId?: string
  prUrl?: string
  onNavigateToPreview?: () => void
  onDismiss: () => void
  className?: string
  variant?: "banner" | "snackbar"
}

export const PrNotificationBanner: React.FC<PrNotificationBannerProps> = ({
  message,
  prId,
  prUrl,
  onNavigateToPreview,
  onDismiss,
  className = "",
  variant = "banner",
}) => {
  if (variant === "snackbar") {
    return (
      <div
        role="status"
        aria-label="Snackbar Notification"
        className={`fixed bottom-6 right-6 z-50 max-w-md bg-slate-900/95 backdrop-blur-md border border-purple-500/40 shadow-2xl shadow-black/60 rounded-2xl p-4 text-slate-200 flex items-center justify-between gap-4 animate-in fade-in slide-in-from-bottom-5 duration-300 ${className}`.trim()}
      >
        <div className="flex items-center space-x-3">
          <div className="p-2 bg-purple-500/10 text-purple-400 rounded-lg border border-purple-500/20 shrink-0">
            <CheckCircle2 className="h-5 w-5" />
          </div>
          <span className="text-xs text-purple-200 font-medium">{message}</span>
        </div>
        <div className="flex items-center gap-3 shrink-0">
          {onNavigateToPreview && prId && (
            <button
              type="button"
              onClick={onNavigateToPreview}
              className="px-2.5 py-1 rounded-lg bg-purple-600 hover:bg-purple-500 text-white font-semibold text-xs transition-colors cursor-pointer"
            >
              Go to PR →
            </button>
          )}
          {prUrl && (
            <a
              href={prUrl}
              target="_blank"
              rel="noreferrer"
              className="text-emerald-200 hover:text-white underline text-xs font-semibold"
            >
              View on Forge
            </a>
          )}
          <button
            type="button"
            onClick={onDismiss}
            aria-label="Dismiss notification"
            className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
          >
            <X className="h-4 w-4" />
          </button>
        </div>
      </div>
    )
  }

  return (
    <div
      role="status"
      className={`px-4 py-2.5 rounded-xl bg-emerald-500/10 border border-emerald-500/20 text-xs text-emerald-300 flex items-center justify-between gap-3 ${className}`.trim()}
    >
      <span>{message}</span>
      <div className="flex items-center gap-3 shrink-0">
        {onNavigateToPreview && prId && (
          <button
            type="button"
            onClick={onNavigateToPreview}
            className="px-2.5 py-1 rounded-lg bg-purple-600 hover:bg-purple-500 text-white font-semibold transition-colors cursor-pointer"
          >
            Go to PR →
          </button>
        )}
        {prUrl && (
          <a
            href={prUrl}
            target="_blank"
            rel="noreferrer"
            className="text-emerald-200 hover:text-white underline font-semibold"
          >
            View on Forge
          </a>
        )}
        <button
          type="button"
          onClick={onDismiss}
          className="text-emerald-400 hover:text-emerald-200 underline cursor-pointer"
        >
          Dismiss
        </button>
      </div>
    </div>
  )
}
