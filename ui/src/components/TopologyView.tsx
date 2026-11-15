import React, { useCallback, useEffect, useState } from "react"
import {
  Server,
  Cloud,
  Cpu,
  Layers,
  CheckCircle2,
  AlertTriangle,
  RefreshCw,
  HardDrive,
  Shield,
  Activity,
  Boxes,
  GitPullRequest,
  RotateCw,
  Terminal,
  GitBranch,
  Package,
  Pencil,
  Trash2,
} from "lucide-react"
import {
  ProviderRegistryEntry,
  ReleaseSummary,
  StackSummary,
} from "../types/api"
import {
  deleteRelease,
  deleteStack,
  getProposals,
  getProviders,
  getReleases,
  getStacksWithMeta,
  restartStackWorkspace,
  toggleProvider,
} from "../lib/api"
import { isDemoMode } from "../lib/utils"
import { StackInspectorModal } from "./StackInspectorModal"

const BOOTSTRAP_MANAGEMENT_STACK: StackSummary = {
  id: "stack-mgmt-01",
  name: "management-cluster-core",
  tenant: "keel-system",
  provider: "kind",
  type: "management",
  status: "synced",
  resourceCount: 24,
  lastSynced: "2 minutes ago",
}

const DEFAULT_STACKS: StackSummary[] = [
  BOOTSTRAP_MANAGEMENT_STACK,
  {
    id: "stack-pve-01",
    name: "pve-workload-cluster-prod",
    tenant: "tenant-acme",
    provider: "proxmox",
    type: "workload",
    status: "synced",
    resourceCount: 18,
    lastSynced: "5 minutes ago",
  },
  {
    id: "stack-aws-01",
    name: "aws-us-east-1-cluster",
    tenant: "tenant-globex",
    provider: "aws",
    type: "cloud",
    status: "reconciling",
    resourceCount: 42,
    lastSynced: "1 minute ago",
  },
  {
    id: "stack-hetzner-01",
    name: "hetzner-fsn1-nodes",
    tenant: "tenant-stark",
    provider: "hetzner",
    type: "workload",
    status: "drift",
    resourceCount: 12,
    lastSynced: "12 minutes ago",
  },
]

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

function ensureManagementStack(stackList: StackSummary[]): StackSummary[] {
  if (
    stackList.some(
      (s) => s.type === "management" || s.name === "management-cluster-core"
    )
  ) {
    return stackList
  }
  return [BOOTSTRAP_MANAGEMENT_STACK, ...stackList]
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

export interface TopologyViewProps {
  onNavigateToCatalog?: () => void
  onNavigateToPreview?: () => void
  onEditRelease?: (namespace: string, name: string) => void
  onEditStack?: (namespace: string, name: string) => void
  authenticated?: boolean
}

export const TopologyView: React.FC<TopologyViewProps> = ({
  onNavigateToCatalog,
  onNavigateToPreview,
  onEditRelease,
  onEditStack,
  authenticated = true,
}) => {
  const [stacks, setStacks] = useState<StackSummary[]>(() =>
    isDemoMode() ? DEFAULT_STACKS : []
  )
  const [releases, setReleases] = useState<ReleaseSummary[]>([])
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
  const [autoRefresh, setAutoRefresh] = useState(true)
  const [togglingId, setTogglingId] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [prNotification, setPrNotification] = useState<{
    id: string
    branch: string
  } | null>(null)
  const [filterProvider, setFilterProvider] = useState<string>("all")
  const [inspectedStack, setInspectedStack] = useState<{
    namespace: string
    name: string
    kind?: "stack" | "release"
  } | null>(null)
  const [restartingStackKey, setRestartingStackKey] = useState<string | null>(
    null
  )
  const [deletingKey, setDeletingKey] = useState<string | null>(null)
  const [stackBanner, setStackBanner] = useState<{
    message: string
    prUrl?: string
    prId?: string
  } | null>(null)
  const [releaseBanner, setReleaseBanner] = useState<{
    message: string
    prUrl?: string
    prId?: string
  } | null>(null)

  const resolveStackNamespace = (stack: StackSummary) =>
    stack.namespace ||
    (stack.name === "management-cluster-core"
      ? "flux-system"
      : stack.tenant || "default")

  const handleQuickRestart = async (stack: StackSummary) => {
    const ns = resolveStackNamespace(stack)
    const key = `${ns}/${stack.name}`
    setRestartingStackKey(key)
    setStackBanner(null)
    try {
      const res = await restartStackWorkspace(ns, stack.name)
      setStackBanner({ message: res.message })
      window.setTimeout(fetchTopology, 1500)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to restart stack workspace"
      )
    } finally {
      setRestartingStackKey(null)
    }
  }

  const handleDeleteStack = async (stack: StackSummary) => {
    const ns = resolveStackNamespace(stack)
    const key = `stack:${ns}/${stack.name}`
    setDeletingKey(key)
    setStackBanner(null)
    setError(null)
    try {
      const res = await deleteStack(ns, stack.name)
      setStackBanner({
        message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for stack ${stack.name}`,
        prUrl: res.proposal.prUrl,
        prId: res.proposal.id,
      })
      window.setTimeout(fetchTopology, 1000)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to open delete stack PR"
      )
    } finally {
      setDeletingKey(null)
    }
  }

  const handleDeleteRelease = async (rel: ReleaseSummary) => {
    const ns = rel.namespace || "tenant-default"
    const key = `release:${ns}/${rel.name}`
    setDeletingKey(key)
    setReleaseBanner(null)
    setError(null)
    try {
      const res = await deleteRelease(ns, rel.name)
      setReleaseBanner({
        message: `Opened Delete PR #${res.proposal.id} (${res.branch}) for release ${rel.name}`,
        prUrl: res.proposal.prUrl,
        prId: res.proposal.id,
      })
      window.setTimeout(fetchTopology, 1000)
    } catch (err: unknown) {
      setError(
        err instanceof Error ? err.message : "Failed to open delete release PR"
      )
    } finally {
      setDeletingKey(null)
    }
  }

  const extractPendingProviderPrs = (
    proposals: { id: string; branch: string; status: string }[]
  ) => {
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

  const fetchTopology = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const [stacksRes, providersData, proposalsData, releasesData] =
        await Promise.all([
          getStacksWithMeta(),
          getProviders(),
          getProposals().catch(() => []),
          getReleases().catch(() => []),
        ])
      setStacks(ensureManagementStack(stacksRes.stacks || []))
      setClusterMeta({
        gitRepoUrl: stacksRes.gitRepoUrl,
        forgeProvider: stacksRes.forgeProvider,
        stateBackend: stacksRes.stateBackend,
      })
      setReleases(releasesData || [])
      if (providersData) {
        setProviders(mergeProviders(providersData))
      }
      setPendingProviderPrs(extractPendingProviderPrs(proposalsData || []))
    } catch (err) {
      setError(
        err instanceof Error
          ? err.message
          : "Failed to load live stack topology"
      )
      if (!isDemoMode()) {
        setStacks(ensureManagementStack([]))
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
      getReleases().catch(() => []),
    ])
      .then(([stacksRes, providersData, proposalsData, releasesData]) => {
        if (!cancelled) {
          setError(null)
          setStacks(ensureManagementStack(stacksRes.stacks || []))
          setClusterMeta({
            gitRepoUrl: stacksRes.gitRepoUrl,
            forgeProvider: stacksRes.forgeProvider,
            stateBackend: stacksRes.stateBackend,
          })
          setReleases(releasesData || [])
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
              : "Failed to load live stack topology"
          )
          if (!isDemoMode()) {
            setStacks(ensureManagementStack([]))
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

  useEffect(() => {
    if (!authenticated || !autoRefresh) {
      return
    }
    const timer = window.setInterval(fetchTopology, 5000)
    return () => window.clearInterval(timer)
  }, [authenticated, autoRefresh, fetchTopology])

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

  const filteredStacks =
    filterProvider === "all"
      ? stacks
      : stacks.filter((s) => s.provider === filterProvider)

  const normalizeStatus = (status?: string) => (status ?? "").toLowerCase()

  const syncedCount = stacks.filter(
    (s) => normalizeStatus(s.status) === "synced"
  ).length
  const reconcilingCount = stacks.filter((s) => {
    const st = normalizeStatus(s.status)
    return st === "reconciling" || st === "progressing"
  }).length
  const driftCount = stacks.filter((s) => {
    const norm = normalizeStatus(s.status)
    return (
      norm === "drift" ||
      norm === "drifted" ||
      norm === "error" ||
      norm === "failed" ||
      norm === "deleting"
    )
  }).length

  return (
    <div className="space-y-6">
      {/* Overview header stats */}
      <div className="flex flex-col md:flex-row md:items-center justify-between gap-4 bg-slate-900 border border-slate-800 p-6 rounded-2xl">
        <div>
          <h2 className="text-xl font-bold text-white flex items-center gap-2">
            <Boxes className="h-6 w-6 text-indigo-400" />
            Cluster &amp; Provider Topology Map
          </h2>
          <p className="text-xs text-slate-400 mt-1">
            View your management cluster, provisioned workload stacks, and
            enabled infrastructure providers
          </p>
        </div>
        <div className="flex items-center space-x-2">
          <button
            type="button"
            onClick={() => setAutoRefresh((v) => !v)}
            className={`px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors cursor-pointer ${
              autoRefresh
                ? "bg-indigo-600/20 text-indigo-300 border-indigo-500/40"
                : "bg-slate-800 text-slate-400 border-slate-700"
            }`}
          >
            {autoRefresh ? "Auto: 5s" : "Auto: Off"}
          </button>
          <button
            onClick={fetchTopology}
            disabled={loading}
            className="flex items-center space-x-2 px-3 py-1.5 bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 rounded-lg text-xs transition-colors cursor-pointer"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            <span>Refresh Map</span>
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
            {isDemoMode() ? " (showing fallback local topology)" : ""}
          </span>
        </div>
      )}

      {prNotification && (
        <div
          role="status"
          className="p-4 bg-indigo-950/40 border border-indigo-500/30 rounded-xl text-indigo-300 text-xs flex items-center justify-between"
        >
          <div className="flex items-center space-x-2">
            <GitPullRequest className="h-4 w-4 text-indigo-400 shrink-0" />
            <span>
              PR #{prNotification.id} opened on branch {prNotification.branch}
            </span>
          </div>
          <button
            type="button"
            onClick={() => setPrNotification(null)}
            className="text-slate-400 hover:text-slate-200"
          >
            &times;
          </button>
        </div>
      )}

      {/* Stats row */}
      <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Total Stacks</p>
            <p className="text-2xl font-bold text-white mt-1">
              {stacks.length}
            </p>
          </div>
          <div className="p-3 bg-indigo-500/10 text-indigo-400 rounded-xl">
            <Layers className="h-6 w-6" />
          </div>
        </div>

        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Synced &amp; Healthy</p>
            <p className="text-2xl font-bold text-emerald-400 mt-1">
              {syncedCount}
            </p>
          </div>
          <div className="p-3 bg-emerald-500/10 text-emerald-400 rounded-xl">
            <CheckCircle2 className="h-6 w-6" />
          </div>
        </div>

        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Reconciling</p>
            <p className="text-2xl font-bold text-blue-400 mt-1">
              {reconcilingCount}
            </p>
          </div>
          <div className="p-3 bg-blue-500/10 text-blue-400 rounded-xl">
            <RefreshCw
              className={`h-6 w-6 ${reconcilingCount > 0 ? "animate-spin" : ""}`}
            />
          </div>
        </div>

        <div className="bg-slate-900 border border-slate-800 p-4 rounded-xl flex items-center justify-between">
          <div>
            <p className="text-xs text-slate-400">Drifted / Error / Deleting</p>
            <p className="text-2xl font-bold text-amber-400 mt-1">
              {driftCount}
            </p>
          </div>
          <div className="p-3 bg-amber-500/10 text-amber-400 rounded-xl">
            <AlertTriangle className="h-6 w-6" />
          </div>
        </div>
      </div>

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
                {provider.enabled && (
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

      {/* Provisioned Stacks Table */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Activity className="h-5 w-5 text-indigo-400" />
              Provisioned Stacks &amp; Workload Clusters
            </h3>
            <p className="text-xs text-slate-400">
              Live Pulumi/OpenTofu stacks managed by PKO v2
            </p>
          </div>

          <div className="flex items-center space-x-2">
            <label htmlFor="provider-filter" className="text-xs text-slate-400">
              Filter Provider:
            </label>
            <select
              id="provider-filter"
              value={filterProvider}
              onChange={(e) => setFilterProvider(e.target.value)}
              className="bg-slate-950 border border-slate-800 text-xs text-slate-200 rounded-lg px-3 py-1.5 focus:outline-none focus:ring-1 focus:ring-indigo-500"
            >
              <option value="all">All Providers</option>
              <option value="proxmox">Proxmox VE</option>
              <option value="aws">AWS</option>
              <option value="hetzner">Hetzner</option>
              <option value="kind">kind / k3s</option>
            </select>
          </div>
        </div>

        <div className="overflow-x-auto">
          <table className="w-full text-left text-xs text-slate-300">
            <thead className="bg-slate-950 text-slate-400 border-b border-slate-800 uppercase font-mono">
              <tr>
                <th className="py-3 px-4">Stack Name</th>
                <th className="py-3 px-4">Tenant</th>
                <th className="py-3 px-4">Provider</th>
                <th className="py-3 px-4">Type</th>
                <th className="py-3 px-4">Bundle</th>
                <th className="py-3 px-4">Resources</th>
                <th className="py-3 px-4">Status</th>
                <th className="py-3 px-4">Last Synced</th>
                <th className="py-3 px-4 text-right">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-800/60 font-mono">
              {filteredStacks.map((stack) => (
                <tr
                  key={stack.id}
                  className="hover:bg-slate-800/40 transition-colors"
                >
                  <td className="py-3.5 px-4 font-semibold text-slate-100">
                    {stack.name}
                  </td>
                  <td className="py-3.5 px-4 text-indigo-400">
                    {stack.tenant}
                  </td>
                  <td className="py-3.5 px-4 uppercase text-slate-400">
                    {stack.provider}
                  </td>
                  <td className="py-3.5 px-4 capitalize text-slate-400">
                    {stack.type}
                  </td>
                  <td className="py-3.5 px-4">
                    {stack.bundle ? (
                      <span
                        title={
                          stack.bundleItem
                            ? `Bundle: ${stack.bundle} (item: ${stack.bundleItem})`
                            : `Bundle: ${stack.bundle}`
                        }
                        className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-[10px] font-mono bg-purple-500/15 text-purple-300 border border-purple-500/30"
                      >
                        <span>{stack.bundle}</span>
                        {stack.bundleItem && (
                          <span className="text-purple-400/80">
                            /{stack.bundleItem}
                          </span>
                        )}
                      </span>
                    ) : (
                      <span className="text-slate-600">—</span>
                    )}
                  </td>
                  <td className="py-3.5 px-4 font-bold text-slate-200">
                    {stack.resourceCount}
                  </td>
                  <td className="py-3.5 px-4">
                    {(() => {
                      const normStatus = normalizeStatus(stack.status)
                      return (
                        <span
                          className={`inline-flex items-center space-x-1 px-2.5 py-0.5 rounded-full text-[11px] font-medium ${
                            normStatus === "synced"
                              ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
                              : normStatus === "reconciling" ||
                                  normStatus === "progressing"
                                ? "bg-blue-500/10 text-blue-400 border border-blue-500/20"
                                : normStatus === "pending"
                                  ? "bg-slate-500/10 text-slate-300 border border-slate-500/20"
                                  : normStatus === "deleting"
                                    ? "bg-orange-500/10 text-orange-300 border border-orange-500/30"
                                    : normStatus === "error" ||
                                        normStatus === "failed"
                                      ? "bg-red-500/10 text-red-400 border border-red-500/20"
                                      : "bg-amber-500/10 text-amber-400 border border-amber-500/20"
                          }`}
                        >
                          {(normStatus === "reconciling" ||
                            normStatus === "progressing" ||
                            normStatus === "deleting") && (
                            <RefreshCw className="h-3 w-3 animate-spin" />
                          )}
                          <span>
                            {normStatus === "synced"
                              ? "Synced"
                              : normStatus === "reconciling" ||
                                  normStatus === "progressing"
                                ? "Reconciling"
                                : normStatus === "pending"
                                  ? "Pending"
                                  : normStatus === "deleting"
                                    ? "Deleting"
                                    : normStatus === "error" ||
                                        normStatus === "failed"
                                      ? "Error"
                                      : "Drift Detected"}
                          </span>
                        </span>
                      )
                    })()}
                  </td>
                  <td className="py-3.5 px-4 text-slate-500">
                    {stack.lastSynced}
                  </td>
                  <td className="py-3.5 px-4 text-right">
                    <div className="inline-flex items-center justify-end gap-1.5">
                      {onEditStack && (
                        <button
                          type="button"
                          title={
                            stack.name === "management-cluster-core" ||
                            stack.namespace === "flux-system"
                              ? "Protected control-plane stack"
                              : "Edit"
                          }
                          aria-label={`Edit stack ${stack.name}`}
                          disabled={
                            stack.name === "management-cluster-core" ||
                            stack.namespace === "flux-system"
                          }
                          onClick={() =>
                            onEditStack(
                              resolveStackNamespace(stack),
                              stack.name
                            )
                          }
                          className="p-1.5 rounded-md bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                        >
                          <Pencil className="h-3.5 w-3.5" />
                        </button>
                      )}
                      <button
                        type="button"
                        title="Inspect"
                        aria-label={`Inspect stack ${stack.name}`}
                        onClick={() =>
                          setInspectedStack({
                            namespace: resolveStackNamespace(stack),
                            name: stack.name,
                          })
                        }
                        className="p-1.5 rounded-md bg-indigo-500/10 hover:bg-indigo-500/20 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                      >
                        <Terminal className="h-3.5 w-3.5" />
                      </button>
                      <button
                        type="button"
                        title="Restart Workspace"
                        aria-label={`Restart stack ${stack.name}`}
                        disabled={
                          restartingStackKey ===
                          `${resolveStackNamespace(stack)}/${stack.name}`
                        }
                        onClick={() => handleQuickRestart(stack)}
                        className="p-1.5 rounded-md bg-amber-500/10 hover:bg-amber-500/20 text-amber-300 border border-amber-500/30 transition-colors cursor-pointer disabled:opacity-50"
                      >
                        <RotateCw
                          className={`h-3.5 w-3.5 ${
                            restartingStackKey ===
                            `${resolveStackNamespace(stack)}/${stack.name}`
                              ? "animate-spin"
                              : ""
                          }`}
                        />
                      </button>
                      <button
                        type="button"
                        title={
                          stack.name === "management-cluster-core" ||
                          stack.namespace === "flux-system"
                            ? "Protected control-plane stack"
                            : "Delete (Open PR)"
                        }
                        aria-label={`Delete stack ${stack.name}`}
                        disabled={
                          stack.name === "management-cluster-core" ||
                          stack.namespace === "flux-system" ||
                          deletingKey ===
                            `stack:${resolveStackNamespace(stack)}/${stack.name}`
                        }
                        onClick={() => handleDeleteStack(stack)}
                        className="p-1.5 rounded-md bg-red-500/10 hover:bg-red-500/20 text-red-300 border border-red-500/30 transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
              {filteredStacks.length === 0 && (
                <tr>
                  <td
                    colSpan={7}
                    className="py-8 text-center text-slate-500 font-sans"
                  >
                    {stacks.length === 0
                      ? "No stacks provisioned yet — authenticate and create a proposal from the Catalog."
                      : `No stacks matching provider filter "${filterProvider}".`}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        {stackBanner && (
          <div className="px-6 py-2.5 bg-emerald-500/10 border-t border-emerald-500/20 text-xs text-emerald-300 flex items-center justify-between gap-3">
            <span>{stackBanner.message}</span>
            <div className="flex items-center gap-3 shrink-0">
              {onNavigateToPreview && stackBanner.prId && (
                <button
                  type="button"
                  onClick={onNavigateToPreview}
                  className="px-2.5 py-1 rounded-lg bg-purple-600 hover:bg-purple-500 text-white font-semibold transition-colors cursor-pointer"
                >
                  Go to PR →
                </button>
              )}
              {stackBanner.prUrl && (
                <a
                  href={stackBanner.prUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="text-emerald-200 hover:text-white underline font-semibold"
                >
                  View on Forge
                </a>
              )}
              <button
                type="button"
                onClick={() => setStackBanner(null)}
                className="text-emerald-400 hover:text-emerald-200 underline cursor-pointer"
              >
                Dismiss
              </button>
            </div>
          </div>
        )}
      </div>

      {/* Deployed Application Releases (Flux Helm / Kustomize) */}
      <div className="bg-slate-900 border border-slate-800 rounded-2xl overflow-hidden space-y-4 p-6">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <div>
            <h3 className="text-base font-semibold text-white flex items-center gap-2">
              <Package className="h-5 w-5 text-emerald-400" />
              Deployed Application Releases
            </h3>
            <p className="text-xs text-slate-400">
              Flux HelmReleases &amp; Kustomizations reconciled across tenant
              namespaces
            </p>
          </div>
          {onNavigateToCatalog && (
            <button
              type="button"
              onClick={onNavigateToCatalog}
              className="px-3 py-1.5 rounded-lg text-xs font-medium bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer self-start sm:self-auto"
            >
              + Deploy App
            </button>
          )}
        </div>

        <div className="overflow-x-auto">
          <table className="w-full text-left text-xs text-slate-300">
            <thead className="bg-slate-950 text-slate-400 border-b border-slate-800 uppercase font-mono">
              <tr>
                <th className="py-3 px-4">Release Name</th>
                <th className="py-3 px-4">Namespace</th>
                <th className="py-3 px-4">Target Cluster</th>
                <th className="py-3 px-4">Bundle</th>
                <th className="py-3 px-4">Chart / Source</th>
                <th className="py-3 px-4">Version</th>
                <th className="py-3 px-4">Runtime</th>
                <th className="py-3 px-4">Status</th>
                <th className="py-3 px-4 text-right">Actions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-800/60 font-mono">
              {releases.map((rel) => {
                const norm = rel.status.toLowerCase()
                const badgeClass =
                  norm === "synced" || norm === "ready"
                    ? "bg-emerald-500/10 text-emerald-400 border-emerald-500/20"
                    : norm === "reconciling" || norm === "progressing"
                      ? "bg-blue-500/10 text-blue-400 border-blue-500/20"
                      : norm === "deleting"
                        ? "bg-orange-500/10 text-orange-300 border-orange-500/30"
                        : norm === "error" ||
                            norm === "failed" ||
                            norm === "drift"
                          ? "bg-red-500/10 text-red-400 border-red-500/20"
                          : "bg-slate-500/10 text-slate-300 border-slate-500/20"
                return (
                  <tr
                    key={`${rel.namespace}-${rel.name}`}
                    className="hover:bg-slate-800/40 transition-colors"
                  >
                    <td className="py-3.5 px-4 font-semibold text-slate-100">
                      <div className="flex items-center space-x-2">
                        <span>{rel.name}</span>
                        {(rel.canary || rel.type === "canary") && (
                          <span className="text-[10px] px-1.5 py-0.5 rounded bg-purple-500/15 text-purple-300 border border-purple-500/30">
                            Canary
                          </span>
                        )}
                      </div>
                    </td>
                    <td className="py-3.5 px-4 text-indigo-400">
                      {rel.namespace}
                    </td>
                    <td className="py-3.5 px-4 text-slate-300">
                      {rel.targetCluster ? (
                        <span className="px-2 py-0.5 rounded bg-cyan-500/10 text-cyan-300 border border-cyan-500/20 text-[11px]">
                          {rel.targetCluster.replace(/-kubeconfig$/, "")}
                        </span>
                      ) : (
                        <span className="text-slate-400">local (mgmt)</span>
                      )}
                    </td>
                    <td className="py-3.5 px-4">
                      {rel.bundle ? (
                        <span
                          title={
                            rel.bundleItem
                              ? `Bundle: ${rel.bundle} (item: ${rel.bundleItem})`
                              : `Bundle: ${rel.bundle}`
                          }
                          className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-[10px] font-mono bg-purple-500/15 text-purple-300 border border-purple-500/30"
                        >
                          <span>{rel.bundle}</span>
                          {rel.bundleItem && (
                            <span className="text-purple-400/80">
                              /{rel.bundleItem}
                            </span>
                          )}
                        </span>
                      ) : (
                        <span className="text-slate-600">—</span>
                      )}
                    </td>
                    <td className="py-3.5 px-4 text-slate-300">
                      {rel.chart || rel.name}
                    </td>
                    <td className="py-3.5 px-4 text-slate-400">
                      {rel.version || "latest"}
                    </td>
                    <td className="py-3.5 px-4 uppercase text-slate-400">
                      {rel.runtime || rel.type || "helm"}
                    </td>
                    <td className="py-3.5 px-4">
                      <span
                        className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-[11px] font-medium border capitalize ${badgeClass}`}
                      >
                        {rel.status}
                      </span>
                    </td>
                    <td className="py-3.5 px-4 text-right">
                      <div className="inline-flex items-center justify-end gap-1.5">
                        {onEditRelease && (
                          <button
                            type="button"
                            title="Edit"
                            aria-label={`Edit release ${rel.name}`}
                            onClick={() =>
                              onEditRelease(
                                rel.namespace || "tenant-default",
                                rel.name
                              )
                            }
                            className="p-1.5 rounded-md bg-indigo-600/20 hover:bg-indigo-600/30 text-indigo-300 border border-indigo-500/30 transition-colors cursor-pointer"
                          >
                            <Pencil className="h-3.5 w-3.5" />
                          </button>
                        )}
                        <button
                          type="button"
                          title="Inspect"
                          aria-label={`Inspect release ${rel.name}`}
                          onClick={() =>
                            setInspectedStack({
                              namespace: rel.namespace || "tenant-default",
                              name: rel.name,
                              kind: "release",
                            })
                          }
                          className="p-1.5 rounded-md bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 transition-colors cursor-pointer"
                        >
                          <Terminal className="h-3.5 w-3.5 text-indigo-400" />
                        </button>
                        <button
                          type="button"
                          title="Delete (Open PR)"
                          aria-label={`Delete release ${rel.name}`}
                          disabled={
                            deletingKey ===
                            `release:${rel.namespace || "tenant-default"}/${rel.name}`
                          }
                          onClick={() => handleDeleteRelease(rel)}
                          className="p-1.5 rounded-md bg-red-500/10 hover:bg-red-500/20 text-red-300 border border-red-500/30 transition-colors cursor-pointer disabled:opacity-40"
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    </td>
                  </tr>
                )
              })}
              {releases.length === 0 && (
                <tr>
                  <td
                    colSpan={8}
                    className="py-8 text-center text-slate-500 font-sans"
                  >
                    No application releases deployed yet — deploy a Helm chart
                    from the Catalog.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        {releaseBanner && (
          <div className="px-6 py-2.5 bg-emerald-500/10 border-t border-emerald-500/20 text-xs text-emerald-300 flex items-center justify-between gap-3">
            <span>{releaseBanner.message}</span>
            <div className="flex items-center gap-3 shrink-0">
              {onNavigateToPreview && releaseBanner.prId && (
                <button
                  type="button"
                  onClick={onNavigateToPreview}
                  className="px-2.5 py-1 rounded-lg bg-purple-600 hover:bg-purple-500 text-white font-semibold transition-colors cursor-pointer"
                >
                  Go to PR →
                </button>
              )}
              {releaseBanner.prUrl && (
                <a
                  href={releaseBanner.prUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="text-emerald-200 hover:text-white underline font-semibold"
                >
                  View on Forge
                </a>
              )}
              <button
                type="button"
                onClick={() => setReleaseBanner(null)}
                className="text-emerald-400 hover:text-emerald-200 underline cursor-pointer"
              >
                Dismiss
              </button>
            </div>
          </div>
        )}
      </div>

      {inspectedStack && (
        <StackInspectorModal
          namespace={inspectedStack.namespace}
          name={inspectedStack.name}
          kind={inspectedStack.kind}
          onClose={() => setInspectedStack(null)}
          onRestarted={fetchTopology}
        />
      )}
    </div>
  )
}
