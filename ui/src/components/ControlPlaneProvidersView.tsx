import React, { useCallback, useEffect, useState } from "react"
import {
  Server,
  Cloud,
  RefreshCw,
  AlertTriangle,
  ExternalLink,
  Package,
  RotateCcw,
  SlidersHorizontal,
} from "lucide-react"
import {
  CatalogItem,
  ProviderRegistryEntry,
  ReleaseSummary,
} from "../types/api"
import {
  getCoreAddons,
  getProposals,
  getProviders,
  getReleases,
  getStacksWithMeta,
  rollbackManagementAddon,
  toggleProvider,
} from "../lib/api"
import { formatForgeProvider, isDemoMode } from "../lib/utils"
import { DeployInstanceModal } from "./DeployInstanceModal"
import { PrNotificationBanner } from "./PrNotificationBanner"
import {
  DEFAULT_CORE_ADDONS,
  DEFAULT_PROVIDERS,
  ProviderInfo,
} from "../lib/demoFixtures"

function findAddonRelease(
  addon: CatalogItem,
  releases: ReleaseSummary[]
): ReleaseSummary | undefined {
  return releases.find(
    (r) =>
      r.name === addon.id ||
      r.id === addon.id ||
      (addon.chart && (r.name === addon.chart || r.chart === addon.chart))
  )
}

function getAddonStatusBadge(release?: ReleaseSummary): {
  label: "Ready" | "Pending" | "Failed" | "Unknown"
  className: string
} {
  const raw = release?.status?.toLowerCase()
  if (raw === "healthy" || raw === "ready") {
    return {
      label: "Ready",
      className:
        "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20",
    }
  }
  if (raw === "progressing" || raw === "pending") {
    return {
      label: "Pending",
      className: "bg-amber-500/10 text-amber-400 border border-amber-500/20",
    }
  }
  if (raw === "failed" || raw === "degraded") {
    return {
      label: "Failed",
      className: "bg-red-500/10 text-red-400 border border-red-500/20",
    }
  }
  return {
    label: "Unknown",
    className: "bg-slate-800 text-slate-400 border border-slate-700",
  }
}

async function safeGetCoreAddons(): Promise<CatalogItem[]> {
  try {
    return await getCoreAddons()
  } catch {
    return []
  }
}

async function safeGetReleases(): Promise<ReleaseSummary[]> {
  try {
    return await getReleases()
  } catch {
    return []
  }
}

function mergeProviders(liveEntries: ProviderRegistryEntry[]): ProviderInfo[] {
  if (!liveEntries || liveEntries.length === 0) {
    return DEFAULT_PROVIDERS
  }
  const knownIds = new Set(DEFAULT_PROVIDERS.map((p) => p.id))
  const updatedDefaults = DEFAULT_PROVIDERS.map((def) => {
    const live = liveEntries.find((e) => e.id === def.id)
    return live
      ? {
          ...def,
          enabled: live.enabled,
          name: live.description || def.name,
        }
      : def
  })
  const extras: ProviderInfo[] = liveEntries
    .filter((e) => !knownIds.has(e.id))
    .map((e) => ({
      id: e.id,
      name: e.description || e.id,
      type: e.package || "Infrastructure Provider",
      enabled: e.enabled,
      nodesOrRegions: "Default Region",
    }))
  return [...updatedDefaults, ...extras]
}

function extractPendingProviderPrs(
  proposals: { id: string; branch: string; status: string }[]
) {
  const map: Record<string, { id: string; branch: string }> = {}
  for (const pr of proposals) {
    if (pr.status !== "open") continue
    const match = /^keel\/providers\/toggle-([a-z0-9-]+)-\d+$/.exec(pr.branch)
    if (match) {
      map[match[1]] = { id: pr.id, branch: pr.branch }
    }
  }
  return map
}

export interface ControlPlaneProvidersViewProps {
  onNavigateToCatalog?: () => void
  onNavigateToPreview?: () => void
  authenticated?: boolean
}

export const ControlPlaneProvidersView: React.FC<
  ControlPlaneProvidersViewProps
> = ({ onNavigateToCatalog, onNavigateToPreview, authenticated = true }) => {
  const [clusterMeta, setClusterMeta] = useState<{
    gitRepoUrl?: string
    forgeProvider?: string
    stateBackend?: string
  }>({})
  const [providers, setProviders] = useState<ProviderInfo[]>(() =>
    isDemoMode() ? DEFAULT_PROVIDERS : []
  )
  const [coreAddons, setCoreAddons] = useState<CatalogItem[]>(() =>
    isDemoMode() ? DEFAULT_CORE_ADDONS : []
  )
  const [releases, setReleases] = useState<ReleaseSummary[]>([])
  const [pendingProviderPrs, setPendingProviderPrs] = useState<
    Record<string, { id: string; branch: string }>
  >({})
  const [loading, setLoading] = useState(false)
  const [togglingId, setTogglingId] = useState<string | null>(null)
  const [rollingBackId, setRollingBackId] = useState<string | null>(null)
  const [selectedAddon, setSelectedAddon] = useState<CatalogItem | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [prNotification, setPrNotification] = useState<{
    id: string
    branch: string
    prUrl?: string
  } | null>(null)

  const fetchData = useCallback(async () => {
    setLoading(true)
    try {
      const [
        stacksRes,
        providersData,
        proposalsData,
        coreAddonsData,
        releasesData,
      ] = await Promise.all([
        getStacksWithMeta(),
        getProviders(),
        getProposals().catch(() => []),
        safeGetCoreAddons(),
        safeGetReleases(),
      ])
      setError(null)
      setClusterMeta({
        gitRepoUrl: stacksRes.gitRepoUrl,
        forgeProvider: stacksRes.forgeProvider,
        stateBackend: stacksRes.stateBackend,
      })
      if (providersData) {
        setProviders(mergeProviders(providersData))
      }
      if (coreAddonsData && coreAddonsData.length > 0) {
        setCoreAddons(coreAddonsData)
      } else if (isDemoMode()) {
        setCoreAddons(DEFAULT_CORE_ADDONS)
      } else {
        setCoreAddons([])
      }
      setReleases(releasesData || [])
      setPendingProviderPrs(extractPendingProviderPrs(proposalsData || []))
    } catch (err) {
      const msg =
        err instanceof Error
          ? err.message
          : "Failed to load control plane & provider state"
      if (!(isDemoMode() && msg.includes("API error (401)"))) {
        setError(msg)
      }
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    if (!authenticated) {
      return
    }
    let cancelled = false
    Promise.all([
      getStacksWithMeta(),
      getProviders(),
      getProposals().catch(() => []),
      safeGetCoreAddons(),
      safeGetReleases(),
    ])
      .then(
        ([
          stacksRes,
          providersData,
          proposalsData,
          coreAddonsData,
          releasesData,
        ]) => {
          if (!cancelled) {
            setError(null)
            setClusterMeta({
              gitRepoUrl: stacksRes.gitRepoUrl,
              forgeProvider: stacksRes.forgeProvider,
              stateBackend: stacksRes.stateBackend,
            })
            if (providersData) {
              setProviders(mergeProviders(providersData))
            }
            if (coreAddonsData && coreAddonsData.length > 0) {
              setCoreAddons(coreAddonsData)
            } else if (isDemoMode()) {
              setCoreAddons(DEFAULT_CORE_ADDONS)
            } else {
              setCoreAddons([])
            }
            setReleases(releasesData || [])
            setPendingProviderPrs(
              extractPendingProviderPrs(proposalsData || [])
            )
          }
        }
      )
      .catch((err) => {
        if (!cancelled) {
          const msg =
            err instanceof Error
              ? err.message
              : "Failed to load control plane & provider state"
          if (!(isDemoMode() && msg.includes("API error (401)"))) {
            setError(msg)
          }
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [authenticated])

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
      setError(
        err instanceof Error ? err.message : `Failed to toggle provider ${id}`
      )
    } finally {
      setTogglingId(null)
    }
  }

  const handleRollbackAddon = async (id: string) => {
    setRollingBackId(id)
    setError(null)
    try {
      const proposal = await rollbackManagementAddon(id)
      setPrNotification({
        id: proposal.id,
        branch: proposal.branch || "main",
        prUrl: proposal.prUrl,
      })
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : `Failed to create rollback PR for ${id}`
      )
    } finally {
      setRollingBackId(null)
    }
  }

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex flex-col md:flex-row md:items-center justify-between gap-4 bg-slate-900 border border-slate-800 p-6 rounded-2xl">
        <div>
          <h2 className="text-xl font-bold text-white flex items-center gap-2">
            <Server className="h-6 w-6 text-indigo-400" />
            Control Plane &amp; Providers
          </h2>
          <p className="text-xs text-slate-400 mt-1">
            Inspect the protected management cluster core and enable or disable
            infrastructure providers
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={fetchData}
            disabled={loading}
            className="flex items-center space-x-2 px-3 py-1.5 bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 rounded-lg text-xs transition-colors cursor-pointer"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            <span>Refresh</span>
          </button>
        </div>
      </div>

      {error && authenticated && (
        <div
          role="alert"
          className="p-4 bg-amber-950/40 border border-amber-500/30 rounded-xl text-amber-300 text-xs flex items-center space-x-2"
        >
          <AlertTriangle className="h-4 w-4 text-amber-400 shrink-0" />
          <span>
            {error}
            {isDemoMode() ? " (showing fallback local providers)" : ""}
          </span>
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

      {/* Management Cluster Core Card */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6">
        <div className="flex items-center justify-between gap-4">
          <div className="flex items-center space-x-3">
            <div className="p-3 bg-indigo-600 rounded-xl text-white shadow-lg shadow-indigo-600/30">
              <Server className="h-6 w-6" />
            </div>
            <div>
              <div className="flex items-center space-x-2">
                <h3 className="text-lg font-semibold text-white">
                  Management Control Plane Cluster
                </h3>
                <span className="text-xs px-2.5 py-0.5 rounded-full bg-indigo-500/10 text-indigo-400 border border-indigo-500/30 font-medium">
                  protect: true
                </span>
              </div>
              <p className="text-xs text-slate-400 mt-1">
                GitOps: Flux CD v2 &middot; Forge:{" "}
                {formatForgeProvider(clusterMeta.forgeProvider)} &middot;
                Secrets: SOPS + age / S3
              </p>
            </div>
          </div>
          <div className="flex items-center space-x-2 shrink-0">
            <span className="w-2.5 h-2.5 rounded-full bg-emerald-400 animate-pulse"></span>
            <span className="text-xs font-semibold text-emerald-400">
              Healthy
            </span>
          </div>
        </div>
      </div>

      {/* Enabled Infrastructure Providers Grid */}
      <div className="space-y-3">
        <h3 className="text-sm font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-2">
          <Cloud className="h-4 w-4 text-indigo-400" /> Enabled Provider Mesh
        </h3>
        <div className="grid grid-cols-1 md:grid-cols-3 lg:grid-cols-5 gap-4">
          {providers.map((provider) => (
            <div
              key={provider.id}
              className={`flex flex-col justify-between h-full bg-slate-900 border p-4 rounded-xl transition-all ${
                provider.enabled
                  ? "border-slate-800 hover:border-indigo-500/60"
                  : "border-slate-800/60 opacity-75"
              }`}
            >
              <div className="space-y-2">
                <div className="flex items-start justify-between gap-2">
                  <span className="text-xs font-semibold text-white leading-snug">
                    {provider.name}
                  </span>
                  <span
                    className={`shrink-0 text-[10px] px-2 py-0.5 rounded-full font-medium ${
                      provider.enabled
                        ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                        : "bg-slate-800 text-slate-500 border border-slate-700"
                    }`}
                  >
                    {provider.enabled ? "enabled" : "disabled"}
                  </span>
                </div>
                <p className="text-[11px] text-slate-400">{provider.type}</p>
                <p className="text-[10px] font-mono text-slate-500 truncate">
                  {provider.nodesOrRegions}
                </p>
              </div>
              <div className="mt-3 pt-2 border-t border-slate-800/60 flex items-center justify-between gap-2">
                {pendingProviderPrs[provider.id] ? (
                  <button
                    type="button"
                    onClick={onNavigateToPreview}
                    className="text-[11px] px-2.5 py-1 rounded-md font-medium bg-purple-500/10 text-purple-300 border border-purple-500/30 hover:bg-purple-500/20 transition-colors cursor-pointer"
                  >
                    Go to PR #{pendingProviderPrs[provider.id].id} →
                  </button>
                ) : (
                  <button
                    type="button"
                    onClick={() =>
                      handleToggleProvider(provider.id, provider.enabled)
                    }
                    disabled={togglingId === provider.id}
                    className={`text-[11px] px-2.5 py-1 rounded-md font-medium transition-colors ${
                      provider.enabled
                        ? "bg-rose-500/10 text-rose-400 border border-rose-500/20 hover:bg-rose-500/20 cursor-pointer"
                        : "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20 hover:bg-emerald-500/20 cursor-pointer"
                    } ${togglingId === provider.id ? "opacity-50 cursor-wait" : ""}`}
                  >
                    {togglingId === provider.id
                      ? "Updating..."
                      : provider.enabled
                        ? "Disable"
                        : "Enable"}
                  </button>
                )}
                {provider.enabled && onNavigateToCatalog && (
                  <button
                    type="button"
                    onClick={onNavigateToCatalog}
                    className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 flex items-center gap-1 shrink-0 cursor-pointer"
                  >
                    Provision &rarr;
                  </button>
                )}
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* Management Plane Base Addons */}
      <div className="space-y-3">
        <div className="flex items-center justify-between">
          <h3 className="text-sm font-semibold text-slate-300 uppercase tracking-wider flex items-center gap-2">
            <Package className="h-4 w-4 text-emerald-400" /> Management Plane
            Base Addons
          </h3>
          <span className="text-[11px] text-slate-500">
            Protected core HelmReleases (PR-driven upgrades &amp; rollbacks)
          </span>
        </div>

        {coreAddons.length === 0 ? (
          <div className="bg-slate-900 border border-slate-800 rounded-xl p-6 text-center text-xs text-slate-400">
            No management plane base addons found.
          </div>
        ) : (
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
            {coreAddons.map((addon) => {
              const release = findAddonRelease(addon, releases)
              const statusBadge = getAddonStatusBadge(release)
              const displayChart = release?.chart || addon.chart || addon.id
              const displayVersion = release?.version || addon.version
              return (
                <div
                  key={addon.id}
                  data-testid={`core-addon-${addon.id}`}
                  className="flex flex-col justify-between bg-slate-900 border border-slate-800 hover:border-indigo-500/50 p-4 rounded-xl transition-all space-y-3"
                >
                  <div className="space-y-2">
                    <div className="flex items-start justify-between gap-2">
                      <div className="min-w-0">
                        <h4 className="text-sm font-semibold text-white truncate">
                          {addon.name}
                        </h4>
                        <p className="text-[11px] font-mono text-slate-500 truncate">
                          {addon.id}
                        </p>
                      </div>
                      <div className="flex items-center gap-1.5 shrink-0">
                        <span
                          title="keel.io/protect: true"
                          className="text-[10px] px-2 py-0.5 rounded-full font-medium bg-indigo-500/10 text-indigo-300 border border-indigo-500/30"
                        >
                          Protected
                        </span>
                        <span
                          className={`text-[10px] px-2 py-0.5 rounded-full font-medium ${statusBadge.className}`}
                        >
                          {statusBadge.label}
                        </span>
                      </div>
                    </div>

                    {addon.description && (
                      <p className="text-[11px] text-slate-400 line-clamp-2">
                        {addon.description}
                      </p>
                    )}

                    <div className="flex items-center gap-2 text-[11px] font-mono text-slate-400 pt-1">
                      <span className="px-2 py-0.5 rounded bg-slate-950 border border-slate-800 truncate">
                        {displayChart}
                      </span>
                      <span className="px-2 py-0.5 rounded bg-slate-950 border border-slate-800 text-slate-300 shrink-0">
                        v{displayVersion.replace(/^v/i, "")}
                      </span>
                    </div>

                    {addon.serviceUrl && (
                      <div className="bg-slate-950 p-2.5 rounded-lg border border-slate-800 text-xs">
                        <p className="text-[10px] uppercase tracking-wider text-slate-500">
                          Service
                        </p>
                        <a
                          href={addon.serviceUrl}
                          target="_blank"
                          rel="noreferrer"
                          className="mt-0.5 inline-flex items-center gap-1.5 text-[11px] font-mono text-indigo-400 hover:text-indigo-300 break-all"
                        >
                          <span>{addon.serviceUrl}</span>
                          <ExternalLink className="h-3 w-3 shrink-0" />
                        </a>
                      </div>
                    )}
                  </div>

                  <div className="pt-3 border-t border-slate-800/60 flex items-center justify-between gap-2">
                    <button
                      type="button"
                      onClick={() => setSelectedAddon(addon)}
                      className="flex items-center gap-1.5 text-[11px] px-2.5 py-1.5 rounded-lg font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                    >
                      <SlidersHorizontal className="h-3 w-3" />
                      <span>Upgrade / Configure</span>
                    </button>
                    <button
                      type="button"
                      onClick={() => handleRollbackAddon(addon.id)}
                      disabled={rollingBackId === addon.id}
                      className="flex items-center gap-1.5 text-[11px] px-2.5 py-1.5 rounded-lg font-medium bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer disabled:opacity-50"
                    >
                      <RotateCcw
                        className={`h-3 w-3 ${rollingBackId === addon.id ? "animate-spin" : ""}`}
                      />
                      <span>
                        {rollingBackId === addon.id
                          ? "Rolling back..."
                          : "Rollback PR"}
                      </span>
                    </button>
                  </div>
                </div>
              )
            })}
          </div>
        )}
      </div>

      <DeployInstanceModal
        isOpen={Boolean(selectedAddon)}
        onClose={() => setSelectedAddon(null)}
        catalogItem={selectedAddon}
        managementAddon={selectedAddon}
        tenant="keel-system"
        onProposalCreated={(proposal) => {
          setPrNotification({
            id: proposal.id,
            branch: proposal.branch || "main",
            prUrl: proposal.prUrl,
          })
        }}
        onCatalogItemUpdated={(updated) => {
          setCoreAddons((prev) =>
            prev.map((a) => (a.id === updated.id ? updated : a))
          )
          setSelectedAddon(updated)
        }}
      />
    </div>
  )
}
