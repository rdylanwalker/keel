import React from "react"

interface PrNotificationBannerProps {
  message: string
  prId?: string
  prUrl?: string
  onNavigateToPreview?: () => void
  onDismiss: () => void
  className?: string
}

export const PrNotificationBanner: React.FC<PrNotificationBannerProps> = ({
  message,
  prId,
  prUrl,
  onNavigateToPreview,
  onDismiss,
  className = "",
}) => (
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
