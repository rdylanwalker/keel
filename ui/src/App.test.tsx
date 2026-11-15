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
  bootstrapLogin: vi.fn(),
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
      phase: "bootstrap",
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

  it("switches navigation tabs across all 4 views", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "bootstrap",
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

    // Tab 4: GitOps Console
    const gitopsTabBtn = screen.getByRole("button", {
      name: /GitOps & Canary Console/i,
    })
    await act(async () => {
      fireEvent.click(gitopsTabBtn)
    })
    expect(
      await screen.findByText("GitOps & Progressive Delivery Console")
    ).toBeInTheDocument()
    expect(window.location.hash).toBe("#gitops")

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

  it("opens login modal, handles successful token authentication, and allows logout", async () => {
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: false,
      phase: "bootstrap",
      tenant: "tenant-default",
    })
    vi.mocked(api.bootstrapLogin).mockResolvedValue({ success: true })

    await act(async () => {
      render(<App />)
    })

    const loginBtn = screen.getByRole("button", {
      name: /Bootstrap Token Login/i,
    })
    fireEvent.click(loginBtn)

    expect(screen.getByText("Stage 0 Bootstrap Auth")).toBeInTheDocument()

    const tokenInput = screen.getByPlaceholderText(/Enter 256-bit token/i)
    fireEvent.change(tokenInput, { target: { value: "token-secret-256" } })

    const authBtn = screen.getByRole("button", { name: /Authenticate Token/i })
    await act(async () => {
      fireEvent.click(authBtn)
    })

    await waitFor(() => {
      expect(screen.getByText("Stage 0 Auth")).toBeInTheDocument()
    })

    // Click logout
    const logoutBtn = screen.getByLabelText("Logout")
    await act(async () => {
      fireEvent.click(logoutBtn)
    })

    expect(api.setStoredToken).toHaveBeenCalledWith(null)
    expect(
      screen.getByRole("button", { name: /Bootstrap Token Login/i })
    ).toBeInTheDocument()
  })

  it("handles auth status failure gracefully on mount", async () => {
    vi.mocked(api.getAuthStatus).mockRejectedValue(new Error("Server offline"))

    await act(async () => {
      render(<App />)
    })

    await waitFor(() => {
      expect(screen.getByText("Stage 0 Unauthenticated")).toBeInTheDocument()
    })
  })

  it("renders demo data across views when demo=true is present in location search", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getAuthStatus).mockResolvedValue({
      authenticated: true,
      phase: "bootstrap",
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
      phase: "bootstrap",
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
})
