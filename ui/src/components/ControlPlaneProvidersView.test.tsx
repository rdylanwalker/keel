import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { ControlPlaneProvidersView } from "./ControlPlaneProvidersView"
import * as api from "../lib/api"

vi.mock("../lib/api", () => ({
  getStacksWithMeta: vi.fn(),
  getProviders: vi.fn(),
  getProposals: vi.fn().mockResolvedValue([]),
  toggleProvider: vi.fn(),
}))

describe("ControlPlaneProvidersView component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(api.getStacksWithMeta).mockResolvedValue({
      stacks: [],
      gitRepoUrl: "https://git.example.com/org/infra.git",
      forgeProvider: "forgejo",
      stateBackend: "s3://keel-state",
    })
    vi.mocked(api.getProviders).mockResolvedValue([])
    vi.mocked(api.getProposals).mockResolvedValue([])
  })

  it("renders management control plane cluster and live providers", async () => {
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
      { id: "gcp", enabled: true, description: "Google Cloud Platform" },
    ])

    render(<ControlPlaneProvidersView />)

    expect(screen.getByText("Control Plane & Providers")).toBeInTheDocument()
    expect(
      screen.getByText("Management Control Plane Cluster")
    ).toBeInTheDocument()

    await waitFor(() => {
      expect(screen.getByText("Google Cloud Platform")).toBeInTheDocument()
      expect(
        screen.getByText("forgejo: https://git.example.com/org/infra.git")
      ).toBeInTheDocument()
    })
  })

  it("toggles provider enable state and displays PR notification banner", async () => {
    const onNavigateToCatalog = vi.fn()
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "gcp", enabled: false, description: "Google Cloud Platform" },
      {
        id: "custom-cloud",
        enabled: true,
        description: "Custom Cloud Provider",
        package: "custom-package",
      },
    ])
    vi.mocked(api.toggleProvider).mockResolvedValueOnce({
      id: "789",
      title: "Enable GCP",
      branch: "feature/enable-gcp",
      author: "user",
      tenant: "keel-system",
      status: "open",
      createdAt: "now",
    })

    render(
      <ControlPlaneProvidersView onNavigateToCatalog={onNavigateToCatalog} />
    )

    await waitFor(() => {
      expect(api.getProviders).toHaveBeenCalled()
      expect(screen.getByText("Custom Cloud Provider")).toBeInTheDocument()
    })

    const provisionBtn = screen.getAllByRole("button", {
      name: /Provision →/i,
    })[0]
    fireEvent.click(provisionBtn)
    expect(onNavigateToCatalog).toHaveBeenCalled()

    const gcpEnableBtn = screen.getAllByRole("button", { name: /^Enable$/i })[0]
    fireEvent.click(gcpEnableBtn)

    await waitFor(() => {
      expect(api.toggleProvider).toHaveBeenCalledWith("gcp", true)
      expect(
        screen.getByText("PR #789 opened on branch feature/enable-gcp")
      ).toBeInTheDocument()
    })

    const closeBannerBtn = screen.getByRole("button", { name: /Dismiss/i })
    fireEvent.click(closeBannerBtn)
    expect(
      screen.queryByText("PR #789 opened on branch feature/enable-gcp")
    ).not.toBeInTheDocument()
  })

  it("handles failure during provider toggle cleanly", async () => {
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
    ])
    vi.mocked(api.toggleProvider).mockRejectedValueOnce("Toggle error string")

    render(<ControlPlaneProvidersView />)

    await waitFor(() => {
      expect(api.getProviders).toHaveBeenCalled()
    })

    const disableBtn = screen.getAllByRole("button", { name: /^Disable$/i })[0]
    fireEvent.click(disableBtn)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to toggle provider proxmox"
      )
    })
  })

  it("refreshes control plane and provider data on Refresh click", async () => {
    render(<ControlPlaneProvidersView />)

    await waitFor(() => {
      expect(api.getProviders).toHaveBeenCalledTimes(1)
    })

    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))

    await waitFor(() => {
      expect(api.getProviders).toHaveBeenCalledTimes(2)
    })
  })

  it("shows Go to PR button for pending provider toggle PRs and handles fetch error", async () => {
    const onNavigateToPreview = vi.fn()
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "gcp", enabled: false, description: "Google Cloud Platform" },
    ])
    vi.mocked(api.getProposals).mockResolvedValueOnce([
      {
        id: "55",
        title: "Toggle gcp",
        branch: "keel/providers/toggle-gcp-12345",
        author: "user",
        tenant: "keel-system",
        status: "open",
        createdAt: "now",
      },
    ])

    render(
      <ControlPlaneProvidersView onNavigateToPreview={onNavigateToPreview} />
    )

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: /Go to PR #55/i })
      ).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /Go to PR #55/i }))
    expect(onNavigateToPreview).toHaveBeenCalled()

    vi.mocked(api.getStacksWithMeta).mockRejectedValueOnce(
      new Error("Backend unreachable")
    )
    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Backend unreachable")
    })
  })
})
