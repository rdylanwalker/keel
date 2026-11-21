import React, { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { Key, LogOut, Menu, ShieldAlert, ShieldCheck } from "lucide-react"

import { AuthStatus, Proposal, TenantSummary } from "./types/api"
import {
  consumePostLoginLocation,
  getAuthStatus,
  getTenants,
  logout,
  setStoredToken,
  startOidcLogin,
} from "./lib/api"
import { isDemoMode } from "./lib/utils"
import { ADMIN_AUTHZ, buildAuthz } from "./lib/authz"
import { AuthzProvider } from "./lib/authz-context"
import {
  ActiveTab,
  CatalogKindFilter,
  formatHashRoute,
  HashRoute,
  parseHashRoute,
} from "./lib/routes"
import { DeployInstanceModal } from "./components/DeployInstanceModal"
import { NavMenu } from "./components/shell/NavMenu"
import { DemoBanner } from "./components/shell/DemoBanner"
import { TenantSelector } from "./components/shell/TenantSelector"
import { ProposalToast } from "./components/shell/ProposalToast"
import { useEditInstance } from "./components/shell/useEditInstance"
import { Sheet, SheetContent, SheetTitle } from "./components/ui/sheet"
import { Button } from "./components/ui/button"
import { Badge } from "./components/ui/badge"

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

export type { ActiveTab }

const DEMO_TENANTS: TenantSummary[] = [
  { id: "default", namespace: "tenant-default" },
  { id: "acme", namespace: "tenant-acme" },
]

/** Hash-routed navigation (`#tab[/param]`), kept in sync with browser history. */
function useHashRoute() {
  const [route, setRoute] = useState<HashRoute>(() =>
    parseHashRoute(window.location.hash)
  )
  useEffect(() => {
    const canonical = formatHashRoute(parseHashRoute(window.location.hash))
    if (window.location.hash !== canonical) {
      window.history.replaceState(null, "", canonical)
    }
    const onChange = () => setRoute(parseHashRoute(window.location.hash))
    window.addEventListener("hashchange", onChange)
    return () => window.removeEventListener("hashchange", onChange)
  }, [])
  const navigate = useCallback((next: HashRoute) => {
    setRoute(next)
    const hash = formatHashRoute(next)
    if (window.location.hash !== hash) {
      window.location.hash = hash
    }
  }, [])
  return { route, navigate }
}

export const App: React.FC = () => {
  const demo = isDemoMode()
  const { route, navigate } = useHashRoute()
  const [authStatus, setAuthStatus] = useState<AuthStatus>({
    authenticated: false,
    phase: "oidc-active",
  })
  const [tenants, setTenants] = useState<TenantSummary[]>([])
  const [selectedTenant, setSelectedTenant] = useState<string>("")
  const [navOpen, setNavOpen] = useState(false)
  const [toastProposal, setToastProposal] = useState<Proposal | null>(null)
  const edit = useEditInstance()

  const navigateToTab = useCallback(
    (tab: ActiveTab) => {
      setNavOpen(false)
      navigate({ tab })
    },
    [navigate]
  )

  useEffect(() => {
    let cancelled = false
    getAuthStatus()
      .then((status) => {
        if (cancelled) return
        setAuthStatus(status)
        if (status.authenticated) {
          // Restore the deep link captured before the OIDC redirect (UI-13).
          const hash = consumePostLoginLocation()
          if (hash && hash !== window.location.hash) {
            window.location.hash = hash
          }
        }
      })
      .catch(() => {
        if (!cancelled) {
          setAuthStatus((prev) => ({ ...prev, authenticated: false }))
        }
      })
    return () => {
      cancelled = true
    }
  }, [])

  useEffect(() => {
    if (!authStatus.authenticated || demo) return
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
  }, [authStatus.authenticated, authStatus.isAdmin, demo])

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

  const authz = useMemo(
    () => (demo ? ADMIN_AUTHZ : buildAuthz(authStatus)),
    [demo, authStatus]
  )

  const handleLogin = () => {
    if (demo) {
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
    setAuthStatus({ authenticated: false, phase: "oidc-active" })
    setTenants([])
    setSelectedTenant("")
    if (!demo) {
      try {
        await logout()
      } catch {
        // Best-effort server cookie clear; local state is already cleared.
      }
    }
  }

  const visibleTenants: TenantSummary[] = demo
    ? DEMO_TENANTS
    : authStatus.authenticated
      ? tenants
      : []
  const activeSelectedTenant =
    authStatus.authenticated || demo ? selectedTenant : ""

  const derivedTenantLabel = (() => {
    if (visibleTenants.length === 1) return visibleTenants[0].namespace
    const groupTenant = authStatus.groups?.find(
      (g) => !["keel-admins", "admin", "admins"].includes(g)
    )
    if (groupTenant) {
      return groupTenant.startsWith("tenant-")
        ? groupTenant
        : `tenant-${groupTenant}`
    }
    return authStatus.tenant || "—"
  })()

  const showTenantSwitcher = visibleTenants.length > 1 || authz.isAdmin
  const userDisplayLabel =
    authStatus.name ||
    authStatus.email ||
    authStatus.sub ||
    authStatus.username ||
    (authStatus.phase === "break-glass" ? "Break-Glass" : "OIDC Auth")
  const canViewApp = authStatus.authenticated || demo

  const openPreview = (id?: string) =>
    navigate({ tab: "preview", proposalId: id })

  // The sidebar sticks directly below the (sticky) header. The header height changes with
  // viewport width (subtitle wrapping) and the demo banner, so track its real bottom edge.
  const headerRef = useRef<HTMLElement | null>(null)
  const [stickyTop, setStickyTop] = useState(0)
  useEffect(() => {
    const el = headerRef.current
    if (!el) return
    const update = () => setStickyTop(el.offsetTop + el.offsetHeight)
    update()
    if (typeof ResizeObserver === "undefined") return
    const ro = new ResizeObserver(update)
    ro.observe(el)
    return () => ro.disconnect()
  }, [demo])

  return (
    <AuthzProvider value={authz}>
      <div className="min-h-screen bg-slate-950 text-slate-100 flex flex-col font-sans antialiased">
        {demo && <DemoBanner />}
        <header
          ref={headerRef}
          className={`border-b border-slate-800 bg-slate-900/90 backdrop-blur sticky ${demo ? "top-7" : "top-0"} z-40 px-3 sm:px-6 py-3 flex items-center justify-between gap-2`}
        >
          <div className="flex items-center gap-2 sm:gap-3 min-w-0">
            <Button
              variant="ghost"
              size="icon"
              className="md:hidden"
              aria-label="Open navigation"
              aria-expanded={navOpen}
              onClick={() => setNavOpen(true)}
            >
              <Menu className="h-5 w-5" />
            </Button>
            <img
              src="/keel-logo.svg"
              alt="Keel Control Plane logo"
              className="h-8 w-8 sm:h-10 sm:w-10 rounded-xl shadow-lg shadow-indigo-600/30 shrink-0"
            />
            <div className="min-w-0">
              <h1 className="text-sm sm:text-lg font-bold tracking-tight text-white flex items-center gap-2">
                <span className="truncate">Keel Control Plane</span>
                <span className="hidden lg:inline text-[10px] font-mono px-2 py-0.5 rounded-full bg-emerald-500/10 text-emerald-400 border border-emerald-500/20">
                  keel.io/v1alpha1
                </span>
              </h1>
              <p className="hidden sm:block text-xs text-slate-400">
                Provider-Agnostic GitOps &amp; Infrastructure Orchestration
              </p>
            </div>
          </div>

          <div className="flex items-center gap-2 min-w-0">
            {canViewApp && (
              <TenantSelector
                tenants={visibleTenants}
                value={activeSelectedTenant}
                onChange={setSelectedTenant}
                switchable={showTenantSwitcher}
                fixedLabel={derivedTenantLabel}
              />
            )}
            {authStatus.authenticated ? (
              <div className="flex items-center gap-1.5">
                <span className="flex items-center gap-2 h-9 text-xs bg-slate-950 text-slate-100 border border-slate-700 px-2.5 sm:px-3 rounded-lg font-medium">
                  <ShieldCheck className="h-3.5 w-3.5 text-emerald-400 shrink-0" />
                  <span className="hidden sm:inline max-w-[12rem] truncate">
                    {userDisplayLabel}
                  </span>
                  {authz.isAdmin ? (
                    <Badge className="rounded-md uppercase tracking-wide">
                      Admin
                    </Badge>
                  ) : (
                    <Badge variant="info" className="rounded-md uppercase">
                      User
                    </Badge>
                  )}
                </span>
                <Button
                  variant="ghost"
                  size="icon"
                  onClick={handleLogout}
                  aria-label="Logout"
                  title="Logout"
                  className="hover:text-red-400"
                >
                  <LogOut className="h-4 w-4" />
                </Button>
              </div>
            ) : demo ? (
              <Button onClick={handleLogin}>
                <Key className="h-3.5 w-3.5" />
                <span>Demo Login</span>
              </Button>
            ) : (
              <span className="flex items-center gap-1.5 text-xs bg-amber-500/10 text-amber-300 border border-amber-500/20 px-3 py-1.5 rounded-lg font-medium">
                <ShieldAlert className="h-3.5 w-3.5 text-amber-400" />
                <span className="hidden sm:inline">Unauthenticated</span>
              </span>
            )}
          </div>
        </header>

        <Sheet open={navOpen} onOpenChange={setNavOpen}>
          <SheetContent aria-describedby={undefined}>
            <SheetTitle className="px-1 pb-2 text-sm font-bold text-white">
              Navigation
            </SheetTitle>
            <NavMenu activeTab={route.tab} onNavigate={navigateToTab} />
          </SheetContent>
        </Sheet>

        <div className="flex-1 flex flex-col md:flex-row min-w-0">
          <aside className="hidden md:block md:w-64 border-r border-slate-800 bg-slate-900/40 shrink-0">
            {/* Pinned below the header while the page scrolls; scrolls itself if taller. */}
            <div
              data-testid="sidebar-sticky"
              className="sticky overflow-y-auto overscroll-contain p-4"
              style={{
                top: stickyTop,
                maxHeight: `calc(100vh - ${stickyTop}px)`,
              }}
            >
              <NavMenu activeTab={route.tab} onNavigate={navigateToTab} />
            </div>
          </aside>

          <main className="flex-1 min-w-0 p-3 sm:p-6 overflow-x-hidden">
            {!canViewApp ? (
              <div className="max-w-lg mx-auto mt-12 bg-slate-900 border border-slate-800 rounded-2xl p-8 text-center space-y-4 shadow-xl">
                <div className="mx-auto w-12 h-12 rounded-2xl bg-indigo-500/10 border border-indigo-500/20 flex items-center justify-center text-indigo-400">
                  <Key className="h-6 w-6" />
                </div>
                <h2 className="text-lg font-bold text-white">
                  Authentication Required
                </h2>
                <p className="text-xs text-slate-400 leading-relaxed">
                  Sign in with your cluster OIDC identity provider (Rauthy or
                  your configured external IdP) to view cluster topology,
                  enabled providers, catalog blueprints, and open pull requests.
                </p>
                <Button size="lg" onClick={handleLogin}>
                  <Key className="h-4 w-4" />
                  <span>Sign in with OIDC</span>
                </Button>
              </div>
            ) : (
              <React.Suspense
                fallback={
                  <div className="flex items-center justify-center p-12 text-slate-400 text-xs">
                    <span>Loading view...</span>
                  </div>
                }
              >
                {route.tab === "topology" && (
                  <TopologyView
                    authenticated={authStatus.authenticated}
                    selectedTenant={activeSelectedTenant || undefined}
                    onNavigateToCatalog={(kind?: CatalogKindFilter) =>
                      navigate({ tab: "catalog", kind })
                    }
                    onNavigateToPreview={() => openPreview()}
                    onNavigateToProviders={() => navigateToTab("providers")}
                    onEditRelease={(ns, name) =>
                      void edit.open(ns, name, "app")
                    }
                    onEditStack={(ns, name) =>
                      void edit.open(ns, name, "infra")
                    }
                  />
                )}
                {route.tab === "catalog" && (
                  <CatalogWizardView
                    selectedTenant={activeSelectedTenant}
                    onNavigateToPreview={() => openPreview()}
                    kindFilter={route.kind}
                    onKindFilterChange={(kind) =>
                      navigate({ tab: "catalog", kind })
                    }
                  />
                )}
                {route.tab === "preview" && (
                  <PreviewInspectorView
                    selectedTenant={activeSelectedTenant || undefined}
                    selectedProposalId={route.proposalId}
                    onSelectProposal={(id) =>
                      navigate({ tab: "preview", proposalId: id ?? undefined })
                    }
                  />
                )}
                {route.tab === "providers" && (
                  <ControlPlaneProvidersView
                    authenticated={authStatus.authenticated}
                    onNavigateToCatalog={() => navigateToTab("catalog")}
                    onNavigateToPreview={() => openPreview()}
                    onNavigateToTopology={() => navigateToTab("topology")}
                  />
                )}
              </React.Suspense>
            )}
          </main>
        </div>

        <DeployInstanceModal
          isOpen={edit.state.open}
          onClose={edit.close}
          catalogItem={edit.state.catalogItem}
          tenant={edit.state.initialConfig?.tenant || "default"}
          stacks={edit.state.stacks}
          releases={edit.state.releases}
          initialConfig={edit.state.initialConfig}
          isEditMode={true}
          loadingConfig={edit.state.loading}
          configError={edit.state.error}
          onProposalCreated={(proposal) => setToastProposal(proposal)}
        />

        {toastProposal && (
          <ProposalToast
            proposal={toastProposal}
            onDismiss={() => setToastProposal(null)}
            onOpen={(id) => {
              setToastProposal(null)
              openPreview(id)
            }}
          />
        )}
      </div>
    </AuthzProvider>
  )
}

export default App
