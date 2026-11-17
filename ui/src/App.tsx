import React, { useEffect, useState } from "react"
import {
  Boxes,
  Layers,
  GitPullRequest,
  Server,
  ShieldCheck,
  ShieldAlert,
  Key,
  LogOut,
  CheckCircle2,
  ArrowRight,
  X,
} from "lucide-react"
import {
  AuthStatus,
  CatalogItem,
  Proposal,
  ReleaseSummary,
  StackSummary,
  TenantSummary,
} from "./types/api"
import {
  getAuthStatus,
  getCatalog,
  getReleaseConfig,
  getReleases,
  getStackConfig,
  getStacks,
  getTenants,
  logout,
  setStoredToken,
  startOidcLogin,
} from "./lib/api"
import { isDemoMode } from "./lib/utils"
import {
  DeployInstanceInitialConfig,
  DeployInstanceModal,
} from "./components/DeployInstanceModal"

const TopologyView = React.lazy(() =>
  import("./components/TopologyView").then((m) => ({ default: m.TopologyView }))
)
const CatalogWizardView = React.lazy(() =>
  import("./components/CatalogWizardView").then((m) => ({
    default: m.CatalogWizardView,
  }))
)
const PreviewInspectorView = React.lazy(() =>
  import("./components/PreviewInspectorView").then((m) => ({
    default: m.PreviewInspectorView,
  }))
)
const ControlPlaneProvidersView = React.lazy(() =>
  import("./components/ControlPlaneProvidersView").then((m) => ({
    default: m.ControlPlaneProvidersView,
  }))
)

export type ActiveTab = "topology" | "catalog" | "preview" | "providers"

const VALID_TABS: readonly ActiveTab[] = [
  "topology",
  "catalog",
  "preview",
  "providers",
]

function parseHashTab(hash: string): ActiveTab {
  const normalized = hash.replace(/^#\/?/, "").trim() as ActiveTab
  return VALID_TABS.includes(normalized) ? normalized : "topology"
}

export const App: React.FC = () => {
  const [activeTab, setActiveTab] = useState<ActiveTab>(() =>
    parseHashTab(window.location.hash)
  )
  const [editModalOpen, setEditModalOpen] = useState(false)
  const [editModalLoading, setEditModalLoading] = useState(false)
  const [editModalError, setEditModalError] = useState<string | null>(null)
  const [editCatalogItem, setEditCatalogItem] = useState<CatalogItem | null>(
    null
  )
  const [editInitialConfig, setEditInitialConfig] =
    useState<DeployInstanceInitialConfig | null>(null)
  const [editStacks, setEditStacks] = useState<StackSummary[]>([])
  const [editReleases, setEditReleases] = useState<ReleaseSummary[]>([])
  const [topologyProposal, setTopologyProposal] = useState<Proposal | null>(
    null
  )

  const navigateToTab = (tab: ActiveTab) => {
    setActiveTab(tab)
    const nextHash = `#${tab}`
    if (window.location.hash !== nextHash) {
      window.location.hash = nextHash
    }
  }

  const openEditInstanceModal = async (
    namespace: string,
    name: string,
    kind: "app" | "infra"
  ) => {
    setEditModalOpen(true)
    setEditModalLoading(true)
    setEditModalError(null)
    setEditCatalogItem(null)
    setEditInitialConfig({ name })

    try {
      const [cfg, catalogItems, stackList, releaseList] = await Promise.all([
        kind === "infra"
          ? getStackConfig(namespace, name)
          : getReleaseConfig(namespace, name),
        getCatalog().catch(() => [] as CatalogItem[]),
        typeof getStacks === "function"
          ? getStacks().catch(() => [] as StackSummary[])
          : Promise.resolve([] as StackSummary[]),
        typeof getReleases === "function"
          ? getReleases().catch(() => [] as ReleaseSummary[])
          : Promise.resolve([] as ReleaseSummary[]),
      ])

      const matched =
        (catalogItems || []).find((item) => item.id === cfg.catalogId) ?? null
      setEditStacks(
        (stackList || []).filter((s) => s.name !== "management-cluster-core")
      )
      setEditReleases(releaseList || [])
      setEditCatalogItem(matched)
      setEditInitialConfig({
        values: cfg.values,
        name: cfg.name || name,
        version: cfg.version || matched?.version || "",
        tenant: cfg.tenant || "default",
        targetCluster: cfg.targetCluster,
        targetNamespace: cfg.targetNamespace,
        canary: cfg.canary,
      })
      if (!matched) {
        setEditModalError(
          `Catalog blueprint "${cfg.catalogId}" was not found for this instance.`
        )
      }
    } catch (err) {
      setEditModalError(
        err instanceof Error
          ? err.message
          : "Failed to load instance configuration"
      )
    } finally {
      setEditModalLoading(false)
    }
  }

  const handleEditRelease = (namespace: string, name: string) => {
    void openEditInstanceModal(namespace, name, "app")
  }

  const handleEditStack = (namespace: string, name: string) => {
    void openEditInstanceModal(namespace, name, "infra")
  }

  useEffect(() => {
    const initialTab = parseHashTab(window.location.hash)
    const expectedHash = `#${initialTab}`
    if (window.location.hash !== expectedHash) {
      window.history.replaceState(null, "", expectedHash)
    }
    const handleHashChange = () => {
      setActiveTab(parseHashTab(window.location.hash))
    }
    window.addEventListener("hashchange", handleHashChange)
    return () => window.removeEventListener("hashchange", handleHashChange)
  }, [])
  const [authStatus, setAuthStatus] = useState<AuthStatus>({
    authenticated: false,
    phase: "oidc-active",
  })
  const [tenants, setTenants] = useState<TenantSummary[]>([])
  const [selectedTenant, setSelectedTenant] = useState<string>("")

  useEffect(() => {
    let cancelled = false
    getAuthStatus()
      .then((status) => {
        if (!cancelled) {
          setAuthStatus(status)
        }
      })
      .catch(() => {
        if (!cancelled) {
          setAuthStatus((prev) => ({
            ...prev,
            authenticated: false,
          }))
        }
      })
    return () => {
      cancelled = true
    }
  }, [])

  useEffect(() => {
    if (!authStatus.authenticated) {
      return
    }
    let cancelled = false
    getTenants()
      .then((items) => {
        if (!cancelled && Array.isArray(items)) {
          setTenants(items)
          if (items.length === 1 && !authStatus.isAdmin) {
            setSelectedTenant(items[0].id)
          }
        }
      })
      .catch(() => {
        // Keep existing tenants on transient error
      })
    return () => {
      cancelled = true
    }
  }, [authStatus.authenticated, authStatus.isAdmin])

  const visibleTenants: TenantSummary[] = authStatus.authenticated
    ? tenants
    : isDemoMode()
      ? [
          { id: "default", namespace: "tenant-default" },
          { id: "acme", namespace: "tenant-acme" },
        ]
      : []
  const activeSelectedTenant =
    authStatus.authenticated || isDemoMode() ? selectedTenant : ""

  useEffect(() => {
    const handleUnauthorized = () => {
      if (!isDemoMode()) {
        setAuthStatus((prev) =>
          prev.authenticated ? { ...prev, authenticated: false } : prev
        )
      }
    }
    window.addEventListener("keel:unauthorized", handleUnauthorized)
    return () =>
      window.removeEventListener("keel:unauthorized", handleUnauthorized)
  }, [])

  const handleLogin = () => {
    if (isDemoMode()) {
      setAuthStatus({
        authenticated: true,
        phase: "oidc-active",
        username: "Demo Admin (admin@keel.local)",
        name: "Demo Admin",
        email: "admin@keel.local",
        isAdmin: true,
      })
      return
    }
    startOidcLogin()
  }

  const handleLogout = async () => {
    setStoredToken(null)
    setAuthStatus({
      authenticated: false,
      phase: "oidc-active",
    })
    setTenants([])
    setSelectedTenant("")
    if (!isDemoMode()) {
      try {
        await logout()
      } catch {
        // Best-effort server cookie clear; local state is already cleared.
      }
    }
  }

  const derivedTenantLabel = (() => {
    if (visibleTenants.length === 1) {
      return visibleTenants[0].namespace
    }
    const groupTenant = authStatus.groups?.find(
      (g) => !["keel-admins", "admin", "admins"].includes(g)
    )
    if (groupTenant) {
      return groupTenant.startsWith("tenant-")
        ? groupTenant
        : `tenant-${groupTenant}`
    }
    return authStatus.tenant || "tenant-default"
  })()

  const showTenantSwitcher =
    visibleTenants.length > 1 || Boolean(authStatus.isAdmin)
  const userDisplayLabel =
    authStatus.name ||
    authStatus.email ||
    authStatus.sub ||
    authStatus.username ||
    (authStatus.phase === "break-glass" ? "Break-Glass" : "OIDC Auth")

  return (
    <div className="min-h-screen bg-slate-950 text-slate-100 flex flex-col font-sans antialiased">
      {/* Header Bar */}
      <header className="border-b border-slate-800 bg-slate-900/90 backdrop-blur sticky top-0 z-40 px-6 py-3 flex items-center justify-between">
        <div className="flex items-center space-x-3">
          <img
            src="/keel-logo.svg"
            alt="Keel Control Plane logo"
            className="h-10 w-10 rounded-xl shadow-lg shadow-indigo-600/30"
          />
          <div>
            <h1 className="text-lg font-bold tracking-tight text-white flex items-center gap-2">
              Keel Control Plane
              <span className="text-[10px] font-mono px-2 py-0.5 rounded-full bg-emerald-500/10 text-emerald-400 border border-emerald-500/20">
                keel.io/v1alpha1
              </span>
            </h1>
            <p className="text-xs text-slate-400">
              Provider-Agnostic GitOps &amp; Infrastructure Orchestration
            </p>
          </div>
        </div>

        {/* Auth status & Tenant header pill */}
        <div className="flex items-center space-x-3">
          {(authStatus.authenticated || isDemoMode()) &&
            (showTenantSwitcher ? (
              <div className="hidden sm:flex items-center space-x-2 h-9 text-xs bg-slate-950 px-3 rounded-lg border border-slate-800">
                <span className="w-2 h-2 rounded-full bg-emerald-400"></span>
                <label
                  htmlFor="header-tenant-select"
                  className="text-slate-400"
                >
                  Tenant:
                </label>
                <select
                  id="header-tenant-select"
                  aria-label="Active Tenant"
                  value={activeSelectedTenant}
                  onChange={(e) => setSelectedTenant(e.target.value)}
                  className="bg-transparent font-mono font-semibold text-slate-200 focus:outline-none cursor-pointer"
                >
                  <option value="" className="bg-slate-900 text-slate-200">
                    All Tenants
                  </option>
                  {visibleTenants.map((t) => (
                    <option
                      key={t.id}
                      value={t.id}
                      className="bg-slate-900 text-slate-200"
                    >
                      {t.id}
                    </option>
                  ))}
                </select>
              </div>
            ) : (
              <div className="hidden sm:flex items-center space-x-2 h-9 text-xs bg-slate-950 px-3 rounded-lg border border-slate-800">
                <span className="w-2 h-2 rounded-full bg-emerald-400"></span>
                <span className="text-slate-400">Tenant:</span>
                <span className="font-mono font-semibold text-slate-200">
                  {derivedTenantLabel}
                </span>
              </div>
            ))}

          {authStatus.authenticated ? (
            <div className="flex items-center space-x-2">
              <span className="flex items-center space-x-2 h-9 text-xs bg-slate-950 text-slate-100 border border-slate-700 px-3 rounded-lg font-medium">
                <ShieldCheck className="h-3.5 w-3.5 text-emerald-400 shrink-0" />
                <span>{userDisplayLabel}</span>
                {authStatus.isAdmin ? (
                  <span className="px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide rounded-md bg-indigo-500/25 text-indigo-200 border border-indigo-400/60">
                    Admin
                  </span>
                ) : (
                  <span className="px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide rounded-md bg-sky-500/20 text-sky-200 border border-sky-400/50">
                    User
                  </span>
                )}
              </span>
              <button
                onClick={handleLogout}
                aria-label="Logout"
                className="p-1.5 text-slate-400 hover:text-red-400 hover:bg-slate-800 rounded-lg transition-colors"
                title="Logout"
              >
                <LogOut className="h-4 w-4" />
              </button>
            </div>
          ) : isDemoMode() ? (
            <button
              onClick={handleLogin}
              className="flex items-center space-x-1.5 px-3.5 py-1.5 bg-indigo-600 hover:bg-indigo-500 text-white rounded-lg text-xs font-semibold transition-colors shadow-lg shadow-indigo-600/20"
            >
              <Key className="h-3.5 w-3.5" />
              <span>Demo Login</span>
            </button>
          ) : (
            <span className="flex items-center space-x-1.5 text-xs bg-amber-500/10 text-amber-300 border border-amber-500/20 px-3 py-1.5 rounded-lg font-medium">
              <ShieldAlert className="h-3.5 w-3.5 text-amber-400" />
              <span>Unauthenticated</span>
            </span>
          )}
        </div>
      </header>

      {/* Main Container Layout */}
      <div className="flex-1 flex flex-col md:flex-row">
        {/* Navigation Sidebar */}
        <aside className="w-full md:w-64 border-r border-slate-800 bg-slate-900/40 p-4 space-y-1 shrink-0">
          <button
            onClick={() => navigateToTab("topology")}
            className={`w-full flex items-center space-x-3 px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all ${
              activeTab === "topology"
                ? "bg-indigo-600 text-white shadow-lg shadow-indigo-600/20"
                : "text-slate-400 hover:text-slate-200 hover:bg-slate-800/60"
            }`}
          >
            <Boxes className="h-4 w-4" />
            <span>Topology Map</span>
          </button>

          <button
            onClick={() => navigateToTab("catalog")}
            className={`w-full flex items-center space-x-3 px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all ${
              activeTab === "catalog"
                ? "bg-indigo-600 text-white shadow-lg shadow-indigo-600/20"
                : "text-slate-400 hover:text-slate-200 hover:bg-slate-800/60"
            }`}
          >
            <Layers className="h-4 w-4" />
            <span>Catalog &amp; Form Wizard</span>
          </button>

          <button
            onClick={() => navigateToTab("preview")}
            className={`w-full flex items-center space-x-3 px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all ${
              activeTab === "preview"
                ? "bg-indigo-600 text-white shadow-lg shadow-indigo-600/20"
                : "text-slate-400 hover:text-slate-200 hover:bg-slate-800/60"
            }`}
          >
            <GitPullRequest className="h-4 w-4" />
            <span>PR &amp; Preview Inspector</span>
          </button>

          <button
            onClick={() => navigateToTab("providers")}
            className={`w-full flex items-center space-x-3 px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all ${
              activeTab === "providers"
                ? "bg-indigo-600 text-white shadow-lg shadow-indigo-600/20"
                : "text-slate-400 hover:text-slate-200 hover:bg-slate-800/60"
            }`}
          >
            <Server className="h-4 w-4" />
            <span>Control Plane &amp; Providers</span>
          </button>
        </aside>

        {/* Content Body */}
        <main className="flex-1 p-6 overflow-x-hidden">
          {!authStatus.authenticated && !isDemoMode() ? (
            <div className="max-w-lg mx-auto mt-12 bg-slate-900 border border-slate-800 rounded-2xl p-8 text-center space-y-4 shadow-xl">
              <div className="mx-auto w-12 h-12 rounded-2xl bg-indigo-500/10 border border-indigo-500/20 flex items-center justify-center text-indigo-400">
                <Key className="h-6 w-6" />
              </div>
              <h2 className="text-lg font-bold text-white">
                Authentication Required
              </h2>
              <p className="text-xs text-slate-400 leading-relaxed">
                Sign in with your cluster OIDC identity provider (Dex or your
                configured external IdP) to view cluster topology, enabled
                providers, catalog blueprints, and open pull requests.
              </p>
              <button
                type="button"
                onClick={handleLogin}
                className="inline-flex items-center space-x-2 px-5 py-2.5 bg-indigo-600 hover:bg-indigo-500 text-white rounded-xl text-xs font-semibold transition-colors shadow-lg shadow-indigo-600/20 cursor-pointer"
              >
                <Key className="h-4 w-4" />
                <span>Sign in with OIDC</span>
              </button>
            </div>
          ) : (
            <React.Suspense
              fallback={
                <div className="flex items-center justify-center p-12 text-slate-400 text-xs">
                  <span>Loading view...</span>
                </div>
              }
            >
              {activeTab === "topology" && (
                <TopologyView
                  authenticated={authStatus.authenticated}
                  selectedTenant={activeSelectedTenant || undefined}
                  onNavigateToCatalog={() => navigateToTab("catalog")}
                  onNavigateToPreview={() => navigateToTab("preview")}
                  onNavigateToProviders={() => navigateToTab("providers")}
                  onEditRelease={handleEditRelease}
                  onEditStack={handleEditStack}
                />
              )}
              {activeTab === "catalog" && (
                <CatalogWizardView
                  selectedTenant={activeSelectedTenant || undefined}
                  onNavigateToPreview={() => navigateToTab("preview")}
                />
              )}
              {activeTab === "preview" && (
                <PreviewInspectorView
                  selectedTenant={activeSelectedTenant || undefined}
                />
              )}
              {activeTab === "providers" && (
                <ControlPlaneProvidersView
                  authenticated={authStatus.authenticated}
                  onNavigateToCatalog={() => navigateToTab("catalog")}
                  onNavigateToPreview={() => navigateToTab("preview")}
                />
              )}
            </React.Suspense>
          )}
        </main>
      </div>

      {/* In-Place Instance Edit Modal (from Topology) */}
      <DeployInstanceModal
        isOpen={editModalOpen}
        onClose={() => {
          setEditModalOpen(false)
          setEditCatalogItem(null)
          setEditInitialConfig(null)
          setEditModalError(null)
        }}
        catalogItem={editCatalogItem}
        tenant={editInitialConfig?.tenant || "default"}
        stacks={editStacks}
        releases={editReleases}
        initialConfig={editInitialConfig}
        isEditMode={true}
        loadingConfig={editModalLoading}
        configError={editModalError}
        onProposalCreated={(proposal) => setTopologyProposal(proposal)}
      />

      {topologyProposal && (
        <div
          aria-label="Proposal Floating Toast"
          className="fixed bottom-6 right-6 z-50 max-w-md bg-slate-900/95 backdrop-blur-md border border-emerald-500/40 shadow-2xl shadow-black/60 rounded-2xl p-4 text-slate-200 space-y-3"
        >
          <div className="flex items-start justify-between gap-3">
            <div className="flex items-start space-x-2.5 min-w-0">
              <CheckCircle2 className="h-5 w-5 text-emerald-400 shrink-0 mt-0.5" />
              <div className="min-w-0">
                <p className="text-xs font-bold text-white">
                  PR #{topologyProposal.id} Opened
                </p>
                <p className="text-[11px] text-slate-400 truncate font-mono mt-0.5">
                  {topologyProposal.branch}
                </p>
              </div>
            </div>
            <button
              type="button"
              onClick={() => setTopologyProposal(null)}
              aria-label="Dismiss notification"
              className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer shrink-0"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
          <div className="flex items-center justify-end gap-2">
            {topologyProposal.prUrl && (
              <a
                href={topologyProposal.prUrl}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center space-x-1 px-2.5 py-1.5 bg-slate-800 hover:bg-slate-700 text-slate-200 border border-slate-700 rounded-lg text-xs font-medium transition-colors"
              >
                <span>View on Forge</span>
              </a>
            )}
            <button
              type="button"
              onClick={() => {
                setTopologyProposal(null)
                navigateToTab("preview")
              }}
              className="inline-flex items-center space-x-1.5 px-3 py-1.5 bg-purple-600 hover:bg-purple-500 text-white rounded-lg text-xs font-semibold transition-colors cursor-pointer"
            >
              <span>Go to PR</span>
              <ArrowRight className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

export default App
