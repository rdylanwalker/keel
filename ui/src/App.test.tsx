import { render, screen, fireEvent, waitFor, act } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { App } from "./App"
import * as api from "./lib/api"

vi.mock("./lib/api", () => ({
  getAuthStatus: vi.fn(),
  getTenants: vi.fn().mockResolvedValue([]),
  getStacks: vi.fn().mockResolvedValue([]),
  getStacksWithMeta: vi.fn().mockResolvedValue({ stacks: [] }),
  getProviders: vi.fn().mockResolvedValue([]),
  toggleProvider: vi.fn().mockResolvedValue({ id: "1", branch: "main" }),
  getCatalog: vi.fn().mockResolvedValue([]),
  getProposals: vi.fn().mockResolvedValue([]),
  getProposalPreview: vi.fn().mockResolvedValue({
    resources: [],
    summary: { create: 0, update: 0, delete: 0, replace: 0, same: 0, drift: 0 },
  }),
  getReleases: vi.fn().mockResolvedValue([]),
  getReleaseConfig: vi.fn(),
  getStackConfig: vi.fn(),
  createProposal: vi.fn(),
  startOidcLogin: vi.fn(),
  logout: vi.fn().mockResolvedValue({ authenticated: false }),
  setStoredToken: vi.fn(),
  createEventSourceStream: vi.fn(() => ({ close: vi.fn() })),
}))

describe("App top-level application component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    window.history.replaceState(null, "", "/")
  })

  it("renders header, default topology tab, and checks auth status", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: false,
      phase: "oidc-active",
      tenant: "tenant-default",
    })

    await act(async () => {
      render(<App />)
    })

    expect(screen.getByText("Keel Control Plane")).toBeInTheDocument()
    expect(screen.getByText("Topology Map")).toBeInTheDocument()
    expect(
      await screen.findByText("Authentication Required")
    ).toBeInTheDocument()
    expect(
      screen.queryByText("Cluster & Workload Topology")
    ).not.toBeInTheDocument()

    await waitFor(() => {
      expect(api.getAuthStatus).toHaveBeenCalled()
    })
  })

  it("switches navigation tabs across all 4 views", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      tenant: "tenant-acme",
    })

    await act(async () => {
      render(<App />)
    })

    // Tab 2: Catalog
    const catalogTabBtn = screen.getByRole("button", {
      name: /Catalog & Form Wizard/i,
    })
    await act(async () => {
      fireEvent.click(catalogTabBtn)
    })
    expect(
      await screen.findByText("Self-Service Catalog & Dynamic Form Wizard")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#catalog")

    // Tab 3: Preview
    const previewTabBtn = screen.getByRole("button", {
      name: /PR & Preview Inspector/i,
    })
    await act(async () => {
      fireEvent.click(previewTabBtn)
    })
    expect(
      await screen.findByText("PR & PKO v2 Preview Inspector")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#preview")

    // Tab 4: Control Plane & Providers
    const providersTabBtn = screen.getByRole("button", {
      name: /Control Plane & Providers/i,
    })
    await act(async () => {
      fireEvent.click(providersTabBtn)
    })
    expect(
      await screen.findByText("Management Control Plane Cluster")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#providers")

    // Back to Tab 1: Topology
    const topologyTabBtn = screen.getByRole("button", {
      name: /Topology Map/i,
    })
    await act(async () => {
      fireEvent.click(topologyTabBtn)
    })
    expect(
      await screen.findByText("Cluster & Workload Topology")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#topology")
  })

  it("triggers OIDC PKCE login redirect when unauthenticated and supports demo mock login", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: false,
      phase: "oidc-active",
      tenant: "tenant-default",
    })

    await act(async () => {
      render(<App />)
    })

    const loginBtns = screen.getAllByRole("button", {
      name: /Sign in with OIDC/i,
    })
    expect(loginBtns).toHaveLength(1)
    fireEvent.click(loginBtns[0])
    expect(api.startOidcLogin).toHaveBeenCalled()

    // In demo mode, Demo Login sets authenticated state locally without OIDC redirect
    window.history.replaceState({}, "", "/?demo=true")
    await act(async () => {
      render(<App />)
    })
    const demoBtn = screen.getAllByRole("button", { name: /Demo Login/i })[0]
    fireEvent.click(demoBtn)
    expect(screen.getAllByText("Demo Admin").length).toBeGreaterThan(0)

    const logoutBtn = screen.getAllByRole("button", { name: /Logout/i })[0]
    fireEvent.click(logoutBtn)
    expect(api.setStoredToken).toHaveBeenCalledWith(null)
  })

  it("calls server logout endpoint when logging out outside demo mode", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      username: "admin@keel.local",
      tenant: "tenant-default",
    })

    await act(async () => {
      render(<App />)
    })

    const logoutBtn = screen.getAllByRole("button", { name: /Logout/i })[0]
    await act(async () => {
      fireEvent.click(logoutBtn)
    })

    expect(api.setStoredToken).toHaveBeenCalledWith(null)
    expect(api.logout).toHaveBeenCalledTimes(1)
  })

  it("handles auth status failure gracefully on mount", async () => {
    vi.mocked(api.getAuthStatus).mockRejectedValue(new Error("Server offline"))

    await act(async () => {
      render(<App />)
    })

    await waitFor(() => {
      expect(screen.getByText("Unauthenticated")).toBeInTheDocument()
    })
  })

  it("renders demo data across views when demo=true is present in location search", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      tenant: "tenant-acme",
    })

    await act(async () => {
      render(<App />)
    })

    expect(
      await screen.findByText("management-cluster-core")
    ).toBeInTheDocument()
    window.history.replaceState({}, "", "/")
  })

  it("handles auth status with OIDC phase", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc",
      username: "OIDC User",
      tenant: "tenant-acme",
    })

    await act(async () => {
      render(<App />)
    })

    expect(screen.getByText("OIDC User")).toBeInTheDocument()
  })

  it("restores active page from URL hash on initial load and falls back to #topology on unknown hash", async () => {
    window.history.replaceState(null, "", "/#preview")
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      tenant: "tenant-acme",
    })

    const { unmount } = render(<App />)
    expect(
      await screen.findByText("PR & PKO v2 Preview Inspector")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#preview")
    unmount()

    window.history.replaceState(null, "", "/#unknown-page")
    render(<App />)
    expect(
      await screen.findByText("Cluster & Workload Topology")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#topology")
  })

  it("opens in-place edit modal when editing a release from TopologyView", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      tenant: "tenant-acme",
    })
    vi.mocked(api.getReleases).mockResolvedValue([
      {
        id: "tenant-acme/podinfo",
        name: "podinfo",
        namespace: "tenant-acme",
        tenant: "acme",
        type: "helm",
        chart: "podinfo",
        version: "6.5.0",
        status: "synced",
      },
    ])
    vi.mocked(api.getReleaseConfig).mockResolvedValue({
      catalogId: "podinfo",
      name: "podinfo",
      version: "6.5.0",
      tenant: "acme",
      values: { replicaCount: 2 },
    })
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "podinfo",
        name: "Podinfo App",
        description: "Test",
        category: "apps",
        type: "app",
        version: "6.5.0",
        schema: {
          type: "object",
          properties: { replicaCount: { type: "integer" } },
        },
      },
    ])

    await act(async () => {
      render(<App />)
    })

    const editBtn = await screen.findByRole("button", {
      name: /Edit release podinfo/i,
    })
    await act(async () => {
      fireEvent.click(editBtn)
    })

    await waitFor(() => {
      expect(api.getReleaseConfig).toHaveBeenCalledWith(
        "tenant-acme",
        "podinfo"
      )
    })

    vi.mocked(api.createProposal).mockResolvedValueOnce({
      id: "88",
      title: "update podinfo",
      branch: "keel/acme/app-podinfo-88",
      author: "admin",
      tenant: "acme",
      status: "open",
      createdAt: "just now",
      prUrl: "https://git.internal/pr/88",
    })

    const submitBtn = await screen.findByRole("button", {
      name: /Submit & Open Git PR Proposal/i,
    })
    fireEvent.click(submitBtn)

    await waitFor(() => {
      expect(screen.getByText(/PR #88 Opened/i)).toBeInTheDocument()
      expect(screen.getByText(/View on Forge/i)).toBeInTheDocument()
    })

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /Go to PR/i }))
    })
    expect(screen.queryByText(/PR #88 Opened/i)).not.toBeInTheDocument()
    await waitFor(() => {
      expect(api.getProposals).toHaveBeenCalled()
    })
  })

  it("transitions to Authentication Required when keel:unauthorized is dispatched", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      tenant: "tenant-acme",
    })

    await act(async () => {
      render(<App />)
    })

    expect(
      await screen.findByText("Cluster & Workload Topology")
    ).toBeInTheDocument()

    act(() => {
      window.dispatchEvent(new CustomEvent("keel:unauthorized"))
    })

    expect(
      await screen.findByText("Authentication Required")
    ).toBeInTheDocument()
    expect(
      screen.queryByText("Cluster & Workload Topology")
    ).not.toBeInTheDocument()
  })

  it("opens in-place edit modal when editing a stack from TopologyView", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      tenant: "tenant-acme",
    })
    vi.mocked(api.getStacksWithMeta).mockResolvedValue({
      stacks: [
        {
          id: "acme-net",
          name: "acme-net",
          namespace: "tenant-acme",
          tenant: "acme",
          provider: "proxmox",
          type: "workload",
          status: "synced",
          bundle: "web-stack",
        },
      ],
    })
    vi.mocked(api.getStackConfig).mockResolvedValue({
      catalogId: "proxmox-vm",
      name: "acme-net",
      tenant: "acme",
      values: { cores: 4 },
    })
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "proxmox-vm",
        name: "Proxmox VM",
        description: "Test",
        category: "infra",
        type: "infra",
        version: "1.0.0",
        schema: {
          type: "object",
          properties: { cores: { type: "integer" } },
        },
      },
    ])

    await act(async () => {
      render(<App />)
    })

    const editBtn = await screen.findByRole("button", {
      name: /Edit stack acme-net/i,
    })
    await act(async () => {
      fireEvent.click(editBtn)
    })

    await waitFor(() => {
      expect(api.getStackConfig).toHaveBeenCalledWith("tenant-acme", "acme-net")
    })
  })

  it("displays signed-in user identity, Admin badge, and tenant switcher for multi-tenant/admin users", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      sub: "dylan@acme.internal",
      name: "Dylan",
      groups: ["keel-admins"],
      isAdmin: true,
    })
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "default", namespace: "tenant-default" },
      { id: "acme", namespace: "tenant-acme" },
    ])

    await act(async () => {
      render(<App />)
    })

    expect(screen.getByText("Dylan")).toBeInTheDocument()
    expect(screen.getByText("Admin")).toBeInTheDocument()

    const tenantSelect = await screen.findByLabelText("Active Tenant")
    expect(tenantSelect).toHaveValue("")

    await act(async () => {
      fireEvent.change(tenantSelect, { target: { value: "acme" } })
    })

    await waitFor(() => {
      expect(api.getStacksWithMeta).toHaveBeenCalledWith("acme")
      expect(api.getReleases).toHaveBeenCalledWith("acme")
    })
  })

  it("displays static tenant pill derived from single accessible tenant for non-admin user", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "oidc-active",
      sub: "alice@acme.internal",
      groups: ["tenant-acme"],
      isAdmin: false,
    })
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "acme", namespace: "tenant-acme" },
    ])

    await act(async () => {
      render(<App />)
    })

    expect(screen.getByText("alice@acme.internal")).toBeInTheDocument()
    expect(screen.getByText("User")).toBeInTheDocument()
    expect(screen.queryByText("Admin")).not.toBeInTheDocument()
    expect(screen.queryByLabelText("Active Tenant")).not.toBeInTheDocument()
    await waitFor(() => {
      expect(screen.getAllByText("tenant-acme").length).toBeGreaterThan(0)
    })
  })
})
