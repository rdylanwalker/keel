import { render, screen, fireEvent, waitFor, act } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { App } from "./App"
import * as api from "./lib/api"

vi.mock("./lib/api", () => ({
  getAuthStatus: vi.fn(),
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
  startOidcLogin: vi.fn(),
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
      screen.queryByText("Cluster & Provider Topology Map")
    ).not.toBeInTheDocument()

    await waitFor(() => {
      expect(api.getAuthStatus).toHaveBeenCalled()
    })
  })

  it("switches navigation tabs across all 3 views", async () => {
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

    // Back to Tab 1: Topology
    const topologyTabBtn = screen.getByRole("button", {
      name: /Topology Map/i,
    })
    await act(async () => {
      fireEvent.click(topologyTabBtn)
    })
    expect(
      await screen.findByText("Cluster & Provider Topology Map")
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
    fireEvent.click(loginBtns[0])
    expect(api.startOidcLogin).toHaveBeenCalled()

    // In demo mode, Demo Login sets authenticated state locally without OIDC redirect
    window.history.replaceState({}, "", "/?demo=true")
    await act(async () => {
      render(<App />)
    })
    const demoBtn = screen.getAllByRole("button", { name: /Demo Login/i })[0]
    fireEvent.click(demoBtn)
    expect(screen.getAllByText("OIDC Auth").length).toBeGreaterThan(0)

    const logoutBtn = screen.getAllByRole("button", { name: /Logout/i })[0]
    fireEvent.click(logoutBtn)
    expect(api.setStoredToken).toHaveBeenCalledWith(null)
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

    expect(screen.getByText("OIDC Auth")).toBeInTheDocument()
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
      await screen.findByText("Cluster & Provider Topology Map")
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
  })
})
