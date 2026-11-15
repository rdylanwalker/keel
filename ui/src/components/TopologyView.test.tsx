import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { TopologyView } from "./TopologyView"
import * as api from "../lib/api"
import { StackSummary } from "../types/api"

vi.mock("../lib/api", () => ({
  getStacks: vi.fn(),
  getStacksWithMeta: vi.fn(),
  getReleases: vi.fn().mockResolvedValue([]),
  getProviders: vi.fn(),
  getProposals: vi.fn().mockResolvedValue([]),
  toggleProvider: vi.fn(),
}))

const mockLiveStacks: StackSummary[] = [
  {
    id: "stack-live-01",
    name: "live-proxmox-stack",
    tenant: "tenant-acme",
    provider: "proxmox",
    type: "workload",
    status: "synced",
    resourceCount: 10,
    lastSynced: "just now",
  },
  {
    id: "stack-live-02",
    name: "live-aws-stack",
    tenant: "tenant-globex",
    provider: "aws",
    type: "cloud",
    status: "drift",
    resourceCount: 15,
    lastSynced: "5 mins ago",
  },
]

describe("TopologyView component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(api.getProviders).mockResolvedValue([])
    vi.mocked(api.getStacks).mockResolvedValue([])
    vi.mocked(api.getStacksWithMeta).mockImplementation(() =>
      api.getStacks().then((stacks) => ({ stacks }))
    )
    vi.mocked(api.getReleases).mockResolvedValue([])
  })

  it("renders cluster & provider topology map and loads live stacks and providers", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce(mockLiveStacks)
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
      { id: "gcp", enabled: true, description: "Google Cloud Platform" },
    ])

    render(<TopologyView />)

    expect(
      screen.getByText("Cluster & Provider Topology Map")
    ).toBeInTheDocument()
    expect(
      screen.getByText("Management Control Plane Cluster")
    ).toBeInTheDocument()

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
      expect(screen.getByText("live-aws-stack")).toBeInTheDocument()
      // Always includes management-cluster-core by default
      expect(screen.getByText("management-cluster-core")).toBeInTheDocument()
    })
  })

  it("toggles provider enable state and displays PR notification banner", async () => {
    const onNavigateToCatalog = vi.fn()
    vi.mocked(api.getStacks).mockResolvedValueOnce([])
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

    render(<TopologyView onNavigateToCatalog={onNavigateToCatalog} />)

    await waitFor(() => {
      expect(api.getProviders).toHaveBeenCalled()
      expect(screen.getByText("Custom Cloud Provider")).toBeInTheDocument()
    })

    // Click Provision -> on enabled card
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

    // Dismiss PR banner
    const closeBannerBtn = screen.getByRole("button", { name: "×" })
    fireEvent.click(closeBannerBtn)
    expect(
      screen.queryByText("PR #789 opened on branch feature/enable-gcp")
    ).not.toBeInTheDocument()
  })

  it("handles failure during provider toggle cleanly", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce([])
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
    ])
    vi.mocked(api.toggleProvider).mockRejectedValueOnce("Toggle error string")

    render(<TopologyView />)

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

  it("handles filter by provider", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce(mockLiveStacks)

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
    })

    const filterSelect = screen.getByLabelText(/Filter Provider/i)
    fireEvent.change(filterSelect, { target: { value: "proxmox" } })

    expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
    expect(screen.queryByText("live-aws-stack")).not.toBeInTheDocument()

    // Filter by provider with no matching stacks
    fireEvent.change(filterSelect, { target: { value: "hetzner" } })
    expect(
      screen.getByText('No stacks matching provider filter "hetzner".')
    ).toBeInTheDocument()
  })

  it("handles refresh button click and API failure gracefully while preserving management cluster", async () => {
    vi.mocked(api.getStacksWithMeta).mockRejectedValueOnce(
      new Error("Failed to fetch stacks")
    )
    vi.mocked(api.getProviders).mockRejectedValueOnce(
      new Error("Failed to fetch providers")
    )

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to fetch stacks"
      )
    })

    // Management cluster core is always included by default
    expect(screen.getByText("management-cluster-core")).toBeInTheDocument()

    // Click refresh map with non-Error rejection
    vi.mocked(api.getStacks).mockRejectedValueOnce("Unknown network error")
    vi.mocked(api.getProviders).mockResolvedValueOnce([])
    const refreshBtn = screen.getByRole("button", { name: /Refresh Map/i })
    fireEvent.click(refreshBtn)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to load live stack topology"
      )
    })
  })

  it("initializes with DEFAULT_STACKS when demo=true is in location search", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getStacks).mockResolvedValueOnce([])

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByText("management-cluster-core")).toBeInTheDocument()
    })
    window.history.replaceState({}, "", "/")
  })

  it("handles initial mount getStacks/getProviders non-Error rejection in non-demo mode", async () => {
    vi.mocked(api.getStacks).mockRejectedValueOnce("Non error failure string")

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to load live stack topology"
      )
    })
  })

  it("handles initial mount error in demo mode", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getStacks).mockRejectedValueOnce(new Error("Demo mode error"))

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Demo mode error")
    })
    window.history.replaceState({}, "", "/")
  })

  it("handles error during fetchTopology on refresh button click", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce(mockLiveStacks)
    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
    })

    vi.mocked(api.getStacks).mockRejectedValueOnce(new Error("Refresh failed"))
    const refreshBtn = screen.getByRole("button", { name: /Refresh Map/i })
    fireEvent.click(refreshBtn)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Refresh failed")
    })
  })
})
