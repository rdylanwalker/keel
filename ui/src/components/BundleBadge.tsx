import React from "react"

interface BundleBadgeProps {
  bundle?: string | null
  bundleItem?: string | null
  fallback?: React.ReactNode
}

export const BundleBadge: React.FC<BundleBadgeProps> = ({
  bundle,
  bundleItem,
  fallback = null,
}) => {
  if (!bundle) {
    return <>{fallback}</>
  }

  return (
    <span
      title={
        bundleItem
          ? `Bundle: ${bundle} (item: ${bundleItem})`
          : `Bundle: ${bundle}`
      }
      className="inline-flex min-w-0 max-w-[11rem] items-center whitespace-nowrap px-2 py-0.5 rounded-md text-[10px] font-mono bg-purple-500/15 text-purple-300 border border-purple-500/30 shrink-0"
    >
      <span className="min-w-0 truncate">{bundle}</span>
      {bundleItem && (
        <span className="min-w-0 truncate text-purple-400/80">
          /{bundleItem}
        </span>
      )}
    </span>
  )
}
