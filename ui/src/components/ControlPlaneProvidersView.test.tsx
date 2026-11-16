import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { ControlPlaneProvidersView } from "./ControlPlaneProvidersView"
import * as api from "../lib/api"

vi.mock("../lib/api", () => ({
  getStacksWithMeta: vi.fn(),
  getProviders: vi.fn(),
  getProposals: vi.fn().mockResolvedValue([]),
  getCoreAddons: vi.fn().mockResolvedValue([]),
  getReleases: vi.fn().mockResolvedValue([]),
  toggleProvider: vi.fn(),
  rollbackManagementAddon: vi.fn(),
  upgradeManagementAddon: vi.fn(),
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
    vi.mocked(api.getCoreAddons).mockResolvedValue([])
    vi.mocked(api.getReleases).mockResolvedValue([])
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

  it("handles unauthenticated state and initial load rejection", async () => {
    const { rerender } = render(
      <ControlPlaneProvidersView authenticated={false} />
    )
    expect(api.getStacksWithMeta).not.toHaveBeenCalled()

    vi.mocked(api.getStacksWithMeta).mockRejectedValueOnce("String failure")
    rerender(<ControlPlaneProvidersView authenticated={true} />)
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to load control plane & provider state"
      )
    })
  })

  it("renders management plane base addons with live status, upgrade modal, and rollback PR", async () => {
    vi.mocked(api.getCoreAddons).mockResolvedValueOnce([
      {
        id: "kyverno",
        name: "Kyverno",
        description: "Policy engine",
        type: "app",
        category: "Management Addons",
        repoUrl: "https://kyverno.github.io/kyverno/",
        chart: "kyverno",
        version: "3.3.4",
        schema: {
          type: "object",
          properties: {
            replicaCount: { type: "integer", default: 1 },
          },
        },
      },
      {
        id: "flagger",
        name: "Flagger",
        description: "Canary operator",
        type: "app",
        category: "Management Addons",
        chart: "flagger",
        version: "1.40.0",
        schema: { type: "object", properties: {} },
      },
    ])
    vi.mocked(api.getReleases).mockResolvedValueOnce([
      {
        id: "kyverno",
        name: "kyverno",
        tenant: "keel-system",
        namespace: "kyverno",
        type: "helm",
        status: "healthy",
        version: "3.3.4",
        chart: "kyverno",
      },
    ])
    vi.mocked(api.rollbackManagementAddon)
      .mockResolvedValueOnce({
        id: "501",
        title: "Rollback kyverno",
        branch: "keel/mgmt/rollback-kyverno",
        author: "admin",
        tenant: "keel-system",
        status: "open",
        createdAt: "now",
      })
      .mockRejectedValueOnce(
        new Error("No previous revision found for flagger")
      )

    render(<ControlPlaneProvidersView />)

    await waitFor(() => {
      expect(
        screen.getByText("Management Plane Base Addons")
      ).toBeInTheDocument()
      expect(screen.getByTestId("core-addon-kyverno")).toBeInTheDocument()
      expect(screen.getByTestId("core-addon-flagger")).toBeInTheDocument()
    })

    // Status badges, Protected badge, and no Delete button
    expect(screen.getByText("Ready")).toBeInTheDocument()
    expect(screen.getByText("Unknown")).toBeInTheDocument()
    expect(screen.getAllByText("Protected")).toHaveLength(2)
    expect(
      screen.queryByRole("button", { name: /Delete/i })
    ).not.toBeInTheDocument()

    // Upgrade / Configure opens DeployInstanceModal in managementAddon mode
    const upgradeBtns = screen.getAllByRole("button", {
      name: /Upgrade \/ Configure/i,
    })
    fireEvent.click(upgradeBtns[0])
    expect(
      screen.getByRole("dialog", { name: /Upgrade Kyverno/i })
    ).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: /Close modal/i }))

    // Rollback PR succeeds and shows PrNotificationBanner
    const rollbackBtns = screen.getAllByRole("button", {
      name: /Rollback PR/i,
    })
    fireEvent.click(rollbackBtns[0])
    await waitFor(() => {
      expect(api.rollbackManagementAddon).toHaveBeenCalledWith("kyverno")
      expect(
        screen.getByText("PR #501 opened on branch keel/mgmt/rollback-kyverno")
      ).toBeInTheDocument()
    })

    // Rollback PR failure shows error alert
    fireEvent.click(rollbackBtns[1])
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "No previous revision found for flagger"
      )
    })
  })
})
