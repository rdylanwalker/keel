import React, { useCallback, useEffect, useState } from "react"
import {
  Server,
  Cloud,
  Cpu,
  Layers,
  RefreshCw,
  HardDrive,
  Shield,
  GitBranch,
  AlertTriangle,
} from "lucide-react"
import { ProviderRegistryEntry } from "../types/api"
import {
  getProposals,
  getProviders,
  getStacksWithMeta,
  toggleProvider,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"
import { PrNotificationBanner } from "./PrNotificationBanner"

interface ProviderInfo {
  id: string
  name: string
  type: string
  enabled: boolean
  nodesOrRegions: string
}

const DEFAULT_PROVIDERS: ProviderInfo[] = [
  {
    id: "proxmox",
    name: "Proxmox VE Cluster",
    type: "On-Prem Hypervisor",
    enabled: true,
    nodesOrRegions: "3 Nodes (pve-01, pve-02, pve-03)",
  },
  {
    id: "aws",
    name: "Amazon Web Services",
    type: "Public Cloud Provider",
    enabled: true,
    nodesOrRegions: "us-east-1, eu-west-1",
  },
  {
    id: "gcp",
    name: "Google Cloud Platform",
    type: "Public Cloud Provider",
    enabled: false,
    nodesOrRegions: "us-central1",
  },
  {
    id: "hetzner",
    name: "Hetzner Cloud",
    type: "Bare Metal / Cloud",
    enabled: true,
    nodesOrRegions: "fsn1 (Frankfurt)",
  },
  {
    id: "kind",
    name: "Local kind / k3s",
    type: "Stage 0 Bootstrap Target",
    enabled: true,
    nodesOrRegions: "1 Control Plane Node",
  },
]

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
  const [pendingProviderPrs, setPendingProviderPrs] = useState<
    Record<string, { id: string; branch: string }>
  >({})
  const [loading, setLoading] = useState(false)
  const [togglingId, setTogglingId] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [prNotification, setPrNotification] = useState<{
    id: string
    branch: string
    prUrl?: string
  } | null>(null)

  const fetchData = useCallback(async () => {
    setLoading(true)
    try {
      const [stacksRes, providersData, proposalsData] = await Promise.all([
        getStacksWithMeta(),
        getProviders(),
        getProposals().catch(() => []),
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
      setPendingProviderPrs(extractPendingProviderPrs(proposalsData || []))
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : "Failed to load control plane & provider state"
      )
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
    ])
      .then(([stacksRes, providersData, proposalsData]) => {
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
          setPendingProviderPrs(extractPendingProviderPrs(proposalsData || []))
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(
            err instanceof Error
              ? err.message
              : "Failed to load control plane & provider state"
          )
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
      <div className="bg-slate-900 border border-slate-800 rounded-2xl p-6 space-y-4">
        <div className="flex items-center justify-between border-b border-slate-800 pb-4">
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
              <p className="text-xs text-slate-400 mt-0.5">
                Seed GitOps Engine, PKO v2 Synthesizer, Kyverno Gatekeeper,
                Envoy Ingress &amp; Flagger
              </p>
            </div>
          </div>
          <div className="flex items-center space-x-2">
            <span className="w-2.5 h-2.5 rounded-full bg-emerald-400 animate-pulse"></span>
            <span className="text-xs font-semibold text-emerald-400">
              Healthy
            </span>
          </div>
        </div>

        <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-4 text-xs">
          <div className="bg-slate-950 p-3 rounded-lg border border-slate-800">
            <p className="text-slate-500 flex items-center gap-1.5">
              <Cpu className="h-3.5 w-3.5 text-indigo-400" /> GitOps Engine
            </p>
            <p className="font-semibold text-slate-200 mt-1">Flux CD v2</p>
          </div>
          <div className="bg-slate-950 p-3 rounded-lg border border-slate-800">
            <p className="text-slate-500 flex items-center gap-1.5">
              <Layers className="h-3.5 w-3.5 text-purple-400" /> Infra
              Synthesizer
            </p>
            <p className="font-semibold text-slate-200 mt-1">PKO v2</p>
          </div>
          <div className="bg-slate-950 p-3 rounded-lg border border-slate-800">
            <p className="text-slate-500 flex items-center gap-1.5">
              <Shield className="h-3.5 w-3.5 text-emerald-400" /> Policy Engine
            </p>
            <p className="font-semibold text-slate-200 mt-1">Kyverno</p>
          </div>
          <div className="bg-slate-950 p-3 rounded-lg border border-slate-800 min-w-0">
            <p className="text-slate-500 flex items-center gap-1.5">
              <HardDrive className="h-3.5 w-3.5 text-amber-400 shrink-0" />{" "}
              Secrets &amp; State
            </p>
            <p
              className="font-semibold text-slate-200 mt-1 truncate"
              title={clusterMeta.stateBackend || "S3"}
            >
              SOPS + age / {clusterMeta.stateBackend || "S3"}
            </p>
          </div>
          <div className="bg-slate-950 p-3 rounded-lg border border-slate-800 min-w-0 col-span-2 sm:col-span-1">
            <p className="text-slate-500 flex items-center gap-1.5">
              <GitBranch className="h-3.5 w-3.5 text-cyan-400 shrink-0" /> Git
              Forge &amp; Repo
            </p>
            <p
              className="font-semibold text-slate-200 mt-1 truncate font-mono text-[11px]"
              title={clusterMeta.gitRepoUrl || "Not configured"}
            >
              {clusterMeta.forgeProvider
                ? `${clusterMeta.forgeProvider}: `
                : ""}
              {clusterMeta.gitRepoUrl || "Local / Unconfigured"}
            </p>
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
    </div>
  )
}
