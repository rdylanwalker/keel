import React, { useCallback, useEffect, useState } from "react"
import { AlertTriangle, Cloud, RefreshCw, Server } from "lucide-react"

import { ManagementAddonStatus, ProviderRegistryEntry } from "../types/api"
import {
  getManagementAddons,
  getProposals,
  getProviders,
  getStacksWithMeta,
  toggleProvider,
} from "../lib/api"
import { errorMessage, visibleLoadError } from "../lib/errors"
import { formatForgeProvider, isDemoMode } from "../lib/utils"
import { useAuthz } from "../lib/authz-context"
import {
  DEFAULT_PROVIDERS,
  DEMO_MANAGEMENT_ADDON_STATUSES,
} from "../lib/demoFixtures"
import { PrNotificationBanner } from "./PrNotificationBanner"
import { Badge } from "./ui/badge"
import { Button } from "./ui/button"
import { PageHeader } from "./ui/page-header"
import { ProviderCard } from "./providers/ProviderCard"
import {
  deriveManagementHealth,
  extractPendingProviderPrs,
  ManagementHealth,
} from "./providers/addonStatus"

interface ClusterMeta {
  gitRepoUrl?: string
  forgeProvider?: string
  stateBackend?: string
}

interface LoadResult {
  error: string | null
  clusterMeta?: ClusterMeta
  providers?: ProviderRegistryEntry[]
  addonStatuses?: ManagementAddonStatus[] | null
  liveStatusAvailable?: boolean
  pendingProviderPrs?: Record<string, { id: string; branch: string }>
}

const LOAD_ERROR = "Failed to load control plane & provider state"

/**
 * Loads providers (all users) and, for cluster admins only, management-plane data:
 * live add-on Flux status (for the health summary) and pending provider toggle PRs.
 * Management add-ons themselves are listed and upgraded from the Topology Map.
 * Never throws; failures are reported through `error`.
 */
async function loadProvidersState(isAdmin: boolean): Promise<LoadResult> {
  try {
    const [providers, meta] = await Promise.all([
      getProviders(),
      getStacksWithMeta().catch(() => null),
    ])
    const result: LoadResult = {
      error: null,
      providers: providers ?? [],
      clusterMeta: meta
        ? {
            gitRepoUrl: meta.gitRepoUrl,
            forgeProvider: meta.forgeProvider,
            stateBackend: meta.stateBackend,
          }
        : undefined,
    }
    if (isAdmin) {
      const [statuses, proposals] = await Promise.all([
        getManagementAddons().catch(() => null),
        getProposals().catch(() => []),
      ])
      result.addonStatuses = statuses ? statuses.addons : null
      result.liveStatusAvailable = statuses?.liveStatusAvailable ?? false
      result.pendingProviderPrs = extractPendingProviderPrs(proposals ?? [])
    }
    return result
  } catch (err) {
    return { error: visibleLoadError(err, LOAD_ERROR) }
  }
}

const HEALTH_STYLES: Record<
  ManagementHealth["tone"],
  { dot: string; text: string }
> = {
  healthy: { dot: "bg-emerald-400 animate-pulse", text: "text-emerald-400" },
  warning: { dot: "bg-amber-400", text: "text-amber-300" },
  error: { dot: "bg-red-400", text: "text-red-400" },
  unknown: { dot: "bg-slate-500", text: "text-slate-400" },
}

export interface ControlPlaneProvidersViewProps {
  onNavigateToCatalog?: () => void
  onNavigateToPreview?: () => void
  /** Opens the Topology Map, where management add-ons are upgraded and rolled back. */
  onNavigateToTopology?: () => void
  authenticated?: boolean
}

export const ControlPlaneProvidersView: React.FC<
  ControlPlaneProvidersViewProps
> = ({
  onNavigateToCatalog,
  onNavigateToPreview,
  onNavigateToTopology,
  authenticated = true,
}) => {
  const { isAdmin } = useAuthz()
  const demo = isDemoMode()
  const [clusterMeta, setClusterMeta] = useState<ClusterMeta>({})
  const [providers, setProviders] = useState<ProviderRegistryEntry[]>(() =>
    demo ? DEFAULT_PROVIDERS : []
  )
  const [addonStatuses, setAddonStatuses] = useState<
    ManagementAddonStatus[] | null
  >(() => (demo ? DEMO_MANAGEMENT_ADDON_STATUSES : null))
  const [liveStatusAvailable, setLiveStatusAvailable] = useState(demo)
  const [pendingProviderPrs, setPendingProviderPrs] = useState<
    Record<string, { id: string; branch: string }>
  >({})
  const [loading, setLoading] = useState(() => authenticated && !demo)
  const [togglingId, setTogglingId] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [prNotification, setPrNotification] = useState<{
    id: string
    branch: string
    prUrl?: string
  } | null>(null)

  const applyLoadResult = useCallback((result: LoadResult) => {
    setError(result.error)
    if (result.clusterMeta) setClusterMeta(result.clusterMeta)
    if (result.providers) setProviders(result.providers)
    if (result.addonStatuses !== undefined) {
      setAddonStatuses(result.addonStatuses)
      setLiveStatusAvailable(Boolean(result.liveStatusAvailable))
    }
    if (result.pendingProviderPrs) {
      setPendingProviderPrs(result.pendingProviderPrs)
    }
    setLoading(false)
  }, [])

  useEffect(() => {
    if (!authenticated || isDemoMode()) {
      return
    }
    let cancelled = false
    void loadProvidersState(isAdmin).then((result) => {
      if (!cancelled) applyLoadResult(result)
    })
    return () => {
      cancelled = true
    }
  }, [authenticated, isAdmin, applyLoadResult])

  const handleRefresh = async () => {
    if (isDemoMode()) return
    setLoading(true)
    applyLoadResult(await loadProvidersState(isAdmin))
  }

  const handleToggleProvider = async (
    id: string,
    currentlyEnabled: boolean
  ) => {
    setTogglingId(id)
    setError(null)
    try {
      const proposal = await toggleProvider(id, !currentlyEnabled)
      setPrNotification({
        id: proposal.id,
        branch: proposal.branch || "main",
        prUrl: proposal.prUrl,
      })
      setPendingProviderPrs((prev) => ({
        ...prev,
        [id]: { id: proposal.id, branch: proposal.branch || "main" },
      }))
    } catch (err) {
      setError(errorMessage(err, `Failed to toggle provider ${id}`))
    } finally {
      setTogglingId(null)
    }
  }

  const health = deriveManagementHealth(addonStatuses, liveStatusAvailable)

  return (
    <div className="space-y-6">
      <PageHeader
        icon={<Server className="h-6 w-6 text-indigo-400" aria-hidden="true" />}
        title="Control Plane & Providers"
        description={
          isAdmin
            ? "Check management cluster health and enable or disable infrastructure providers"
            : "Infrastructure providers available to your tenants"
        }
        actions={
          <Button
            variant="secondary"
            onClick={handleRefresh}
            disabled={loading}
            aria-busy={loading}
          >
            <RefreshCw
              aria-hidden="true"
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            <span>Refresh</span>
          </Button>
        }
      />

      {error && authenticated && (
        <div
          role="alert"
          className="p-4 bg-amber-950/40 border border-amber-500/30 rounded-xl text-amber-300 text-xs flex items-center space-x-2"
        >
          <AlertTriangle
            className="h-4 w-4 text-amber-400 shrink-0"
            aria-hidden="true"
          />
          <span>{error}</span>
        </div>
      )}

      {prNotification && (
        <PrNotificationBanner
          message={`PR #${prNotification.id} opened on branch ${prNotification.branch}`}
          prId={prNotification.id}
          prUrl={prNotification.prUrl}
          onNavigateToPreview={onNavigateToPreview}
          onDismiss={() => setPrNotification(null)}
        />
      )}

      {/* Management Cluster Core Card (cluster admins only) */}
      {isAdmin && (
        <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6">
          <div className="flex flex-col lg:flex-row lg:items-center justify-between gap-4">
            <div className="flex items-center space-x-3 min-w-0">
              <div className="p-3 bg-indigo-600 rounded-xl text-white shadow-lg shadow-indigo-600/30 shrink-0">
                <Server className="h-6 w-6" aria-hidden="true" />
              </div>
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <h3 className="text-lg font-semibold text-white">
                    Management Control Plane Cluster
                  </h3>
                  <Badge className="text-xs">protect: true</Badge>
                </div>
                <p className="text-xs text-slate-400 mt-1">
                  GitOps: Flux CD v2 &middot; Forge:{" "}
                  {formatForgeProvider(clusterMeta.forgeProvider)} &middot;
                  Secrets: SOPS + age / S3
                </p>
              </div>
            </div>
            <div
              className="flex items-center space-x-2 shrink-0"
              data-testid="management-health"
              title={health.detail}
            >
              <span
                aria-hidden="true"
                className={`w-2.5 h-2.5 rounded-full ${HEALTH_STYLES[health.tone].dot}`}
              ></span>
              <span
                className={`text-xs font-semibold ${HEALTH_STYLES[health.tone].text}`}
              >
                {health.label}
              </span>
              {health.detail && (
                <span className="text-[11px] text-slate-500">
                  ({health.detail})
                </span>
              )}
              {onNavigateToTopology && (
                <button
                  type="button"
                  onClick={onNavigateToTopology}
                  className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 px-2 py-1 cursor-pointer"
                >
                  Manage add-ons in Topology Map &rarr;
                </button>
              )}
            </div>
          </div>
        </div>
      )}

      {/* Infrastructure Providers Grid */}
      <div className="space-y-3">
        <h3 className="text-sm font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-2">
          <Cloud className="h-4 w-4 text-indigo-400" aria-hidden="true" />{" "}
          Enabled Provider Mesh
        </h3>
        {providers.length === 0 ? (
          <div className="bg-slate-900 border border-slate-800 rounded-xl p-6 text-center text-xs text-slate-400">
            {loading
              ? "Loading providers..."
              : "No infrastructure providers registered."}
          </div>
        ) : (
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5 gap-4">
            {providers.map((provider) => (
              <ProviderCard
                key={provider.id}
                provider={provider}
                canManage={isAdmin}
                pendingPr={pendingProviderPrs[provider.id]}
                toggling={togglingId === provider.id}
                onToggle={handleToggleProvider}
                onNavigateToCatalog={onNavigateToCatalog}
                onNavigateToPreview={onNavigateToPreview}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  )
}
