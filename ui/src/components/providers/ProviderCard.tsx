import React from "react"

import { ProviderRegistryEntry } from "../../types/api"
import { Badge } from "../ui/badge"
import { Button } from "../ui/button"

export interface ProviderCardProps {
  provider: ProviderRegistryEntry
  canManage: boolean
  pendingPr?: { id: string; branch: string }
  toggling: boolean
  onToggle: (id: string, currentlyEnabled: boolean) => void
  onNavigateToCatalog?: () => void
  onNavigateToPreview?: () => void
}

/** One provider registry entry; shows only fields reported by `GET /api/v1/providers`. */
export const ProviderCard: React.FC<ProviderCardProps> = ({
  provider,
  canManage,
  pendingPr,
  toggling,
  onToggle,
  onNavigateToCatalog,
  onNavigateToPreview,
}) => {
  const title = provider.description || provider.id
  return (
    <div
      data-testid={`provider-${provider.id}`}
      className={`flex flex-col justify-between h-full bg-slate-900 border p-4 rounded-xl transition-all ${
        provider.enabled
          ? "border-slate-800 hover:border-indigo-500/60"
          : "border-slate-800/60 opacity-75"
      }`}
    >
      <div className="space-y-2">
        <div className="flex items-start justify-between gap-2">
          <span className="text-xs font-semibold text-white leading-snug break-words">
            {title}
          </span>
          <Badge variant={provider.enabled ? "success" : "secondary"}>
            {provider.enabled ? "enabled" : "disabled"}
          </Badge>
        </div>
        {title !== provider.id && (
          <p className="text-[11px] font-mono text-slate-400">{provider.id}</p>
        )}
        {provider.package && (
          <p
            className="text-[10px] font-mono text-slate-500 truncate"
            title={provider.package}
          >
            {provider.package}
          </p>
        )}
      </div>
      {(canManage || (provider.enabled && onNavigateToCatalog)) && (
        <div className="mt-3 pt-2 border-t border-slate-800/60 flex items-center justify-between gap-2">
          {canManage &&
            (pendingPr ? (
              <Button
                variant="subtle"
                size="sm"
                onClick={onNavigateToPreview}
                className="bg-purple-500/10 text-purple-300 border-purple-500/30 hover:bg-purple-500/20"
              >
                Go to PR #{pendingPr.id} →
              </Button>
            ) : (
              <Button
                size="sm"
                variant={provider.enabled ? "destructive-subtle" : "subtle"}
                disabled={toggling}
                aria-busy={toggling}
                onClick={() => onToggle(provider.id, provider.enabled)}
              >
                {toggling
                  ? "Updating..."
                  : provider.enabled
                    ? "Disable"
                    : "Enable"}
              </Button>
            ))}
          {provider.enabled && onNavigateToCatalog && (
            <Button
              variant="link"
              size="sm"
              className="ml-auto px-0"
              onClick={onNavigateToCatalog}
            >
              Provision &rarr;
            </Button>
          )}
        </div>
      )}
    </div>
  )
}
