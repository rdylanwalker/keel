import {
  render,
  screen,
  fireEvent,
  waitFor,
  act,
  within,
} from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { App } from "./App"
import * as api from "./lib/api"
import { useAuthz } from "./lib/authz-context"
import { CatalogKindFilter } from "./lib/routes"

vi.mock("./lib/api", () => ({
  getAuthStatus: vi.fn(),
  getTenants: vi.fn(),
  getCatalog: vi.fn(),
  getStacks: vi.fn(),
  getReleases: vi.fn(),
  getReleaseConfig: vi.fn(),
  getStackConfig: vi.fn(),
  startOidcLogin: vi.fn(),
  logout: vi.fn(),
  consumePostLoginLocation: vi.fn(),
}))

function AuthzProbe() {
  const authz = useAuthz()
  return (
    <span data-testid="authz-probe">
      {authz.isAdmin ? "admin" : "user"}:
      {authz.canAdminTenant("acme") ? "acme-admin" : "acme-none"}
    </span>
  )
}

vi.mock("./components/TopologyView", () => ({
  TopologyView: (p: {
    onNavigateToCatalog: (k?: CatalogKindFilter) => void
    onEditRelease: (ns: string, n: string) => void
    onEditStack: (ns: string, n: string) => void
    onNavigateToProviders: () => void
    selectedTenant?: string
  }) => (
    <div>
      <h2>Topology View</h2>
      <span data-testid="topology-tenant">{p.selectedTenant ?? "all"}</span>
      <AuthzProbe />
      <button onClick={() => p.onNavigateToCatalog("vm")}>deploy vm</button>
      <button onClick={() => p.onEditRelease("tenant-acme", "podinfo")}>
        edit release
      </button>
      <button onClick={() => p.onEditStack("tenant-acme", "vm1")}>
        edit stack
      </button>
      <button onClick={p.onNavigateToProviders}>to providers</button>
    </div>
  ),
}))
vi.mock("./components/CatalogWizardView", () => ({
  CatalogWizardView: (p: {
    kindFilter?: string
    onKindFilterChange: (k?: CatalogKindFilter) => void
  }) => (
    <div>
      <h2>Catalog View</h2>
      <span data-testid="kind-filter">{p.kindFilter ?? "none"}</span>
      <button onClick={() => p.onKindFilterChange(undefined)}>
        clear filter
      </button>
    </div>
  ),
}))
vi.mock("./components/PreviewInspectorView", () => ({
  PreviewInspectorView: (p: {
    selectedProposal?: { scope: string; id: string }
    onSelectProposal: (ref: { scope: string; id: string } | null) => void
  }) => (
    <div>
      <h2>Preview View</h2>
      <span data-testid="selected-pr">
        {p.selectedProposal
          ? `${p.selectedProposal.scope}/${p.selectedProposal.id}`
          : "none"}
      </span>
      <button onClick={() => p.onSelectProposal({ scope: "globex", id: "77" })}>
        select 77
      </button>
    </div>
  ),
}))
vi.mock("./components/ControlPlaneProvidersView", () => ({
  ControlPlaneProvidersView: () => <h2>Providers View</h2>,
}))
vi.mock("./components/DeployInstanceModal", () => ({
  DeployInstanceModal: (p: {
    isOpen: boolean
    loadingConfig?: boolean
    configError?: string | null
    catalogItem: { id: string } | null
    onClose: () => void
    onProposalCreated?: (p: unknown) => void
  }) =>
    p.isOpen ? (
      <div role="dialog" aria-label="edit modal">
        <span>{p.loadingConfig ? "loading" : "ready"}</span>
        <span data-testid="edit-error">{p.configError ?? ""}</span>
        <span data-testid="edit-item">{p.catalogItem?.id ?? ""}</span>
        <button
          onClick={() =>
            p.onProposalCreated?.({
              id: "55",
              title: "t",
              branch: "feat/x",
              tenant: "acme",
              scope: "acme",
              status: "open",
              createdAt: "now",
              prUrl: "https://forge/pr/55",
            })
          }
        >
          create proposal
        </button>
        <button onClick={p.onClose}>close edit</button>
      </div>
    ) : null,
}))

const adminStatus = {
  authenticated: true,
  phase: "oidc-active" as const,
  name: "Alice Admin",
  groups: ["keel-admins"],
  isAdmin: true,
}

async function renderApp() {
  await act(async () => {
    render(<App />)
  })
}

describe("App shell", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    window.history.replaceState(null, "", "/")
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "default", namespace: "tenant-default" },
      { id: "acme", namespace: "tenant-acme" },
    ])
    vi.mocked(api.getCatalog).mockResolvedValue([])
    vi.mocked(api.getStacks).mockResolvedValue([])
    vi.mocked(api.getReleases).mockResolvedValue([])
    vi.mocked(api.consumePostLoginLocation).mockReturnValue(null)
    vi.mocked(api.logout).mockResolvedValue({ authenticated: false })
  })

  it("requires authentication and starts OIDC login", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: false,
      phase: "oidc-active",
    })
    await renderApp()
    expect(screen.getByText("Authentication Required")).toBeInTheDocument()
    expect(screen.getByText("Unauthenticated")).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: /Sign in with OIDC/ }))
    expect(api.startOidcLogin).toHaveBeenCalled()
    expect(screen.queryByTestId("demo-data-banner")).toBeNull()
  })

  it("pins the desktop sidebar below the sticky header while the page scrolls", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    await renderApp()
    const sticky = screen.getByTestId("sidebar-sticky")
    expect(sticky).toHaveClass("sticky", "overflow-y-auto")
    // Height is bounded by the viewport (jsdom normalises the calc() expression).
    expect(sticky.style.maxHeight).toContain("100vh")
    expect(sticky.style.top).toMatch(/^\d+px$/)
    expect(
      within(sticky).getByRole("navigation", { name: "Primary" })
    ).toBeInTheDocument()
  })

  it("treats auth status failure as unauthenticated", async () => {
    vi.mocked(api.getAuthStatus).mockRejectedValue(new Error("down"))
    await renderApp()
    expect(screen.getByText("Authentication Required")).toBeInTheDocument()
  })

  it("navigates between tabs and keeps the hash in sync", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    await renderApp()
    expect(await screen.findByText("Topology View")).toBeInTheDocument()
    expect(window.location.hash).toBe("#topology")
    const nav = screen.getAllByRole("navigation", { name: "Primary" })[0]
    for (const [label, heading, hash] of [
      [/Catalog & Form Wizard/, "Catalog View", "#catalog"],
      [/PR & Preview Inspector/, "Preview View", "#preview"],
      [/Control Plane & Providers/, "Providers View", "#providers"],
      [/Topology Map/, "Topology View", "#topology"],
    ] as const) {
      fireEvent.click(within(nav).getByRole("button", { name: label }))
      expect(await screen.findByText(heading)).toBeInTheDocument()
      expect(window.location.hash).toBe(hash)
      expect(screen.getAllByRole("button", { name: label })[0]).toHaveAttribute(
        "aria-current",
        "page"
      )
    }
  })

  it("routes #catalog/<kind> and #preview/<scope>/<id> deep links", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    window.history.replaceState(null, "", "/#preview/acme/42")
    await renderApp()
    expect(await screen.findByTestId("selected-pr")).toHaveTextContent(
      "acme/42"
    )
    fireEvent.click(screen.getByText("select 77"))
    expect(window.location.hash).toBe("#preview/globex/77")
    await waitFor(() =>
      expect(screen.getByTestId("selected-pr")).toHaveTextContent("globex/77")
    )

    act(() => {
      window.location.hash = "#topology"
    })
    fireEvent.click(await screen.findByText("deploy vm"))
    expect(window.location.hash).toBe("#catalog/vm")
    expect(await screen.findByTestId("kind-filter")).toHaveTextContent("vm")
    fireEvent.click(screen.getByText("clear filter"))
    expect(window.location.hash).toBe("#catalog")
  })

  it("falls back to topology on unknown hashes", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    window.history.replaceState(null, "", "/#nope")
    await renderApp()
    expect(await screen.findByText("Topology View")).toBeInTheDocument()
    expect(window.location.hash).toBe("#topology")
  })

  it("restores the pre-login deep link after authentication", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    vi.mocked(api.consumePostLoginLocation).mockReturnValue(
      "#preview/management/9"
    )
    await renderApp()
    await waitFor(() =>
      expect(window.location.hash).toBe("#preview/management/9")
    )
    expect(await screen.findByTestId("selected-pr")).toHaveTextContent(
      "management/9"
    )
  })

  it("provides admin authz, tenant switcher and Admin badge for admins", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    await renderApp()
    expect(await screen.findByTestId("authz-probe")).toHaveTextContent(
      "admin:acme-admin"
    )
    expect(screen.getByText("Admin")).toBeInTheDocument()
    const select = await screen.findByLabelText("Active Tenant")
    await waitFor(() =>
      expect(select.querySelectorAll("option")).toHaveLength(3)
    )
    fireEvent.change(select, { target: { value: "acme" } })
    expect(screen.getByTestId("topology-tenant")).toHaveTextContent("acme")
  })

  it("derives tenant-admin rights from groups for non-admins", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      email: "bob@acme",
      groups: ["tenant-acme-admin"],
      isAdmin: false,
    })
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "acme", namespace: "tenant-acme" },
    ])
    await renderApp()
    expect(await screen.findByTestId("authz-probe")).toHaveTextContent(
      "user:acme-admin"
    )
    expect(screen.getByText("User")).toBeInTheDocument()
    expect(await screen.findByText("tenant-acme")).toBeInTheDocument()
    await waitFor(() =>
      expect(screen.getByTestId("topology-tenant")).toHaveTextContent("acme")
    )
  })

  it("logs out via the server and returns to the sign-in screen", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    await renderApp()
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Logout" }))
    })
    expect(api.logout).toHaveBeenCalled()
    expect(screen.getByText("Authentication Required")).toBeInTheDocument()
  })

  it("drops to sign-in on keel:unauthorized", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    await renderApp()
    act(() => {
      window.dispatchEvent(new CustomEvent("keel:unauthorized"))
    })
    expect(screen.getByText("Authentication Required")).toBeInTheDocument()
  })

  it("opens the mobile navigation drawer and navigates", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    await renderApp()
    fireEvent.click(screen.getByRole("button", { name: "Open navigation" }))
    const drawer = await screen.findByRole("dialog", { name: "Navigation" })
    fireEvent.click(
      drawer.querySelectorAll("nav button")[3] as HTMLButtonElement
    )
    expect(await screen.findByText("Providers View")).toBeInTheDocument()
    await waitFor(() =>
      expect(screen.queryByRole("dialog", { name: "Navigation" })).toBeNull()
    )
  })

  it("edits a release in place and shows the PR toast", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    vi.mocked(api.getReleaseConfig).mockResolvedValue({
      catalogId: "podinfo",
      tenant: "acme",
      name: "podinfo",
      values: {},
    })
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "podinfo",
        name: "Podinfo",
        description: "",
        type: "app",
        category: "apps",
        version: "1",
        schema: {},
      },
    ])
    await renderApp()
    await act(async () => {
      fireEvent.click(await screen.findByText("edit release"))
    })
    expect(await screen.findByTestId("edit-item")).toHaveTextContent("podinfo")
    fireEvent.click(screen.getByText("create proposal"))
    expect(screen.getByText("PR #55 Opened")).toBeInTheDocument()
    expect(screen.getByRole("link", { name: "View on Forge" })).toHaveAttribute(
      "href",
      "https://forge/pr/55"
    )
    fireEvent.click(screen.getByText("close edit"))
    fireEvent.click(screen.getByRole("button", { name: /Go to PR/ }))
    expect(window.location.hash).toBe("#preview/acme/55")
  })

  it("reports missing blueprints and load errors in the edit modal", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue(adminStatus)
    vi.mocked(api.getStackConfig).mockResolvedValueOnce({
      catalogId: "gone",
      tenant: "acme",
      name: "vm1",
      values: {},
    })
    await renderApp()
    await act(async () => {
      fireEvent.click(await screen.findByText("edit stack"))
    })
    expect(await screen.findByTestId("edit-error")).toHaveTextContent(
      'Catalog blueprint "gone" was not found'
    )
    fireEvent.click(screen.getByText("close edit"))

    vi.mocked(api.getStackConfig).mockRejectedValueOnce(new Error("boom"))
    await act(async () => {
      fireEvent.click(screen.getByText("edit stack"))
    })
    expect(await screen.findByTestId("edit-error")).toHaveTextContent("boom")
  })

  it("demo mode shows the DEMO DATA banner and admin demo login without API calls", async () => {
    window.history.replaceState(null, "", "/?demo=true")
    vi.mocked(api.getAuthStatus).mockRejectedValue(new Error("demo"))
    await renderApp()
    expect(screen.getByTestId("demo-data-banner")).toHaveTextContent(
      /Demo data/i
    )
    expect(screen.getByText("Topology View")).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: /Demo Login/ }))
    expect(screen.getByText("Admin")).toBeInTheDocument()
    expect(api.getTenants).not.toHaveBeenCalled()
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Logout" }))
    })
    expect(api.logout).not.toHaveBeenCalled()
  })
})
