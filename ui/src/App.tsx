import React, { useEffect, useState } from "react"
import {
  Boxes,
  Layers,
  GitPullRequest,
  Activity,
  ShieldCheck,
  ShieldAlert,
  Key,
  LogOut,
} from "lucide-react"
import { AuthStatus } from "./types/api"
import { getAuthStatus, setStoredToken } from "./lib/api"
import { isDemoMode } from "./lib/utils"
import { BootstrapLoginModal } from "./components/BootstrapLoginModal"

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
const GitOpsConsoleView = React.lazy(() =>
  import("./components/GitOpsConsoleView").then((m) => ({
    default: m.GitOpsConsoleView,
  }))
)

export type ActiveTab = "topology" | "catalog" | "preview" | "gitops"

const VALID_TABS: readonly ActiveTab[] = [
  "topology",
  "catalog",
  "preview",
  "gitops",
]

function parseHashTab(hash: string): ActiveTab {
  const normalized = hash.replace(/^#\/?/, "").trim() as ActiveTab
  return VALID_TABS.includes(normalized) ? normalized : "topology"
}

export const App: React.FC = () => {
  const [activeTab, setActiveTab] = useState<ActiveTab>(() =>
    parseHashTab(window.location.hash)
  )
  const [editReleaseTarget, setEditReleaseTarget] = useState<{
    namespace: string
    name: string
    kind?: "app" | "infra"
  } | null>(null)

  const navigateToTab = (tab: ActiveTab) => {
    setActiveTab(tab)
    const nextHash = `#${tab}`
    if (window.location.hash !== nextHash) {
      window.location.hash = nextHash
    }
  }

  const handleEditRelease = (namespace: string, name: string) => {
    setEditReleaseTarget({ namespace, name, kind: "app" })
    navigateToTab("catalog")
  }

  const handleEditStack = (namespace: string, name: string) => {
    setEditReleaseTarget({ namespace, name, kind: "infra" })
    navigateToTab("catalog")
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
    phase: "bootstrap",
    tenant: "tenant-default",
  })
  const [isLoginModalOpen, setIsLoginModalOpen] = useState(false)

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

  const handleLoginSuccess = () => {
    setAuthStatus({
      authenticated: true,
      phase: "bootstrap",
      username: "Stage 0 Bootstrap Admin",
      tenant: "tenant-default",
    })
    setIsLoginModalOpen(false)
  }

  const handleLogout = () => {
    setStoredToken(null)
    setAuthStatus({
      authenticated: false,
      phase: "bootstrap",
      tenant: "tenant-default",
    })
  }

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
          {(authStatus.authenticated || isDemoMode()) && (
            <div className="hidden sm:flex items-center space-x-2 text-xs bg-slate-950 px-3 py-1.5 rounded-lg border border-slate-800">
              <span className="w-2 h-2 rounded-full bg-emerald-400"></span>
              <span className="text-slate-400">Tenant:</span>
              <span className="font-mono font-semibold text-slate-200">
                {authStatus.tenant || "tenant-default"}
              </span>
            </div>
          )}

          {authStatus.authenticated ? (
            <div className="flex items-center space-x-2">
              <span className="flex items-center space-x-1.5 text-xs bg-emerald-500/10 text-emerald-300 border border-emerald-500/20 px-3 py-1.5 rounded-lg font-medium">
                <ShieldCheck className="h-3.5 w-3.5 text-emerald-400" />
                <span>
                  {authStatus.phase === "bootstrap"
                    ? "Stage 0 Auth"
                    : "OIDC Auth"}
                </span>
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
          ) : (
            <button
              onClick={() => setIsLoginModalOpen(true)}
              className="flex items-center space-x-1.5 px-3.5 py-1.5 bg-indigo-600 hover:bg-indigo-500 text-white rounded-lg text-xs font-semibold transition-colors shadow-lg shadow-indigo-600/20"
            >
              <Key className="h-3.5 w-3.5" />
              <span>Bootstrap Token Login</span>
            </button>
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
            onClick={() => navigateToTab("gitops")}
            className={`w-full flex items-center space-x-3 px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all ${
              activeTab === "gitops"
                ? "bg-indigo-600 text-white shadow-lg shadow-indigo-600/20"
                : "text-slate-400 hover:text-slate-200 hover:bg-slate-800/60"
            }`}
          >
            <Activity className="h-4 w-4" />
            <span>GitOps &amp; Canary Console</span>
          </button>

          {/* Unauthenticated info card in sidebar if unauthenticated */}
          {!authStatus.authenticated && (
            <div className="pt-6">
              <div className="bg-slate-950 border border-slate-800 p-4 rounded-xl space-y-2">
                <div className="flex items-center space-x-2 text-amber-400 text-xs font-medium">
                  <ShieldAlert className="h-4 w-4" />
                  <span>Stage 0 Unauthenticated</span>
                </div>
                <p className="text-[11px] text-slate-400 leading-normal">
                  Authenticating via 256-bit token unlocks mutating PR synthesis
                  and merge actions.
                </p>
                <button
                  onClick={() => setIsLoginModalOpen(true)}
                  className="w-full py-1.5 bg-slate-800 hover:bg-slate-700 text-indigo-300 rounded text-xs font-medium transition-colors"
                >
                  Enter Token
                </button>
              </div>
            </div>
          )}
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
                Sign in with the single-use Stage 0 bootstrap token printed by{" "}
                <code className="font-mono text-slate-200 bg-slate-950 px-1.5 py-0.5 rounded">
                  keel serve
                </code>{" "}
                or{" "}
                <code className="font-mono text-slate-200 bg-slate-950 px-1.5 py-0.5 rounded">
                  keel bootstrap init
                </code>{" "}
                to view cluster topology, enabled providers, catalog blueprints,
                and open pull requests.
              </p>
              <button
                type="button"
                onClick={() => setIsLoginModalOpen(true)}
                className="inline-flex items-center space-x-2 px-5 py-2.5 bg-indigo-600 hover:bg-indigo-500 text-white rounded-xl text-xs font-semibold transition-colors shadow-lg shadow-indigo-600/20 cursor-pointer"
              >
                <Key className="h-4 w-4" />
                <span>Enter Bootstrap Token</span>
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
                  onNavigateToCatalog={() => navigateToTab("catalog")}
                  onNavigateToPreview={() => navigateToTab("preview")}
                  onEditRelease={handleEditRelease}
                  onEditStack={handleEditStack}
                />
              )}
              {activeTab === "catalog" && (
                <CatalogWizardView
                  onNavigateToPreview={() => navigateToTab("preview")}
                  initialReleaseTarget={editReleaseTarget}
                  onClearReleaseTarget={() => setEditReleaseTarget(null)}
                />
              )}
              {activeTab === "preview" && <PreviewInspectorView />}
              {activeTab === "gitops" && (
                <GitOpsConsoleView
                  onEditRelease={handleEditRelease}
                  onNavigateToPreview={() => navigateToTab("preview")}
                />
              )}
            </React.Suspense>
          )}
        </main>
      </div>

      {/* Stage 0 Bootstrap Login Modal */}
      <BootstrapLoginModal
        isOpen={isLoginModalOpen}
        onSuccess={handleLoginSuccess}
        onClose={() => setIsLoginModalOpen(false)}
      />
    </div>
  )
}

export default App
