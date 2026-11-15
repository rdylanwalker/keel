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
  createEventSourceStream: vi.fn(() => ({ close: vi.fn() })),
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
    const closeBannerBtn = screen.getByRole("button", { name: /Dismiss/i })
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

  it("filters and sorts stacks via TanStack DataTable", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce(mockLiveStacks)

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
    })

    // Sort by Stack Name
    const stackNameHeader = screen.getByRole("button", { name: /Stack Name/i })
    fireEvent.click(stackNameHeader)
    expect(stackNameHeader.closest("th")).toHaveAttribute(
      "aria-sort",
      "ascending"
    )

    // Filter by provider via table search input
    const filterInput = screen.getByLabelText(
      /Filter stacks by name, tenant, provider, status/i
    )
    fireEvent.change(filterInput, { target: { value: "proxmox" } })

    expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
    expect(screen.queryByText("live-aws-stack")).not.toBeInTheDocument()

    // Filter with no matching stacks
    fireEvent.change(filterInput, { target: { value: "hetzner" } })
    expect(
      screen.getByText("No stacks matching your filter.")
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

  it("counts both stacks and application releases in header status cards", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce([
      {
        id: "stack-1",
        name: "kind-dev",
        tenant: "default",
        provider: "kind",
        type: "workload",
        status: "synced",
        resourceCount: 3,
        lastSynced: "just now",
      },
      {
        id: "stack-2",
        name: "broken-stack",
        tenant: "default",
        provider: "kind",
        type: "workload",
        status: "error",
        resourceCount: 1,
        lastSynced: "just now",
      },
    ])
    vi.mocked(api.getReleases).mockResolvedValueOnce([
      {
        id: "rel-1",
        name: "podinfo",
        namespace: "tenant-default",
        tenant: "default",
        type: "helm",
        status: "Ready",
        version: "6.7.1",
      },
      {
        id: "rel-2",
        name: "grafana",
        namespace: "tenant-default",
        tenant: "default",
        type: "helm",
        status: "Reconciling",
        version: "8.0.0",
      },
      {
        id: "rel-3",
        name: "old-app",
        namespace: "tenant-default",
        tenant: "default",
        type: "helm",
        status: "Deleting",
        version: "1.0.0",
      },
    ])

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByText("kind-dev")).toBeInTheDocument()
      expect(screen.getAllByText("podinfo").length).toBeGreaterThan(0)
    })

    const totalCard = screen.getByText("Total Stacks & Apps").parentElement!
    const syncedCard = screen.getByText("Synced & Healthy").parentElement!
    const reconcilingCard = screen.getAllByText("Reconciling")[0].parentElement!
    const driftCard = screen.getByText(
      "Drifted / Error / Deleting"
    ).parentElement!

    // Includes auto-prepended management-cluster-core (synced)
    expect(totalCard).toHaveTextContent("6")
    expect(syncedCard).toHaveTextContent("3")
    expect(reconcilingCard).toHaveTextContent("1")
    expect(driftCard).toHaveTextContent("2")
  })

  it("renders static SSE Live Stream badge, canary rollouts, and filters/pauses live events", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce([])
    vi.mocked(api.getReleases).mockResolvedValueOnce([
      {
        id: "rel-canary-1",
        name: "podinfo-canary",
        namespace: "tenant-default",
        tenant: "default",
        type: "canary",
        status: "progressing",
        version: "6.7.1",
        canaryWeight: 35,
        errorRate: 0.01,
        latencyMs: 14,
      },
    ])

    let onEventCb:
      ((evt: import("../types/api").CrdWatchEvent) => void) | null = null
    vi.mocked(api.createEventSourceStream).mockImplementation((onEvent) => {
      onEventCb = onEvent
      return { close: vi.fn() } as unknown as EventSource
    })

    render(<TopologyView />)

    expect(await screen.findByText("SSE Live Stream")).toBeInTheDocument()
    expect(screen.getAllByText("podinfo-canary").length).toBeGreaterThan(0)
    expect(screen.getByText("35%")).toBeInTheDocument()

    // Receive live SSE events
    await waitFor(() => expect(onEventCb).not.toBeNull())
    onEventCb!({
      kind: "HelmRelease",
      namespace: "tenant-default",
      name: "podinfo",
      status: "Ready",
      message: "Helm upgrade succeeded",
      timestamp: "now",
    })
    onEventCb!({
      kind: "Stack",
      namespace: "tenant-default",
      name: "kind-dev",
      status: "Ready",
      message: "Stack synced",
      timestamp: "now",
    })

    expect(
      await screen.findByText("Helm upgrade succeeded")
    ).toBeInTheDocument()
    expect(screen.getByText("Stack synced")).toBeInTheDocument()

    // Filter events by CRD kind
    fireEvent.change(screen.getByLabelText(/Filter events by CRD kind/i), {
      target: { value: "Stack" },
    })
    expect(screen.getByText("Stack synced")).toBeInTheDocument()
    expect(screen.queryByText("Helm upgrade succeeded")).not.toBeInTheDocument()

    // Pause stream updates header badge
    fireEvent.click(screen.getByRole("button", { name: /Pause Stream/i }))
    expect(screen.getByText("SSE Paused")).toBeInTheDocument()
    expect(
      screen.getByRole("button", { name: /Resume Stream/i })
    ).toBeInTheDocument()
  })

  it("renders Last Synced for releases and deduplicates identical SSE events", async () => {
    vi.mocked(api.getStacksWithMeta).mockResolvedValue({
      stacks: [
        {
          id: "s-1",
          name: "workload-cluster",
          tenant: "acme",
          provider: "aws",
          type: "cloud",
          status: "synced",
          resourceCount: 14,
          lastSynced: new Date(Date.now() - 120_000).toISOString(),
        },
      ],
    })
    vi.mocked(api.getReleases).mockResolvedValue([
      {
        id: "rel-1",
        name: "podinfo",
        tenant: "acme",
        namespace: "tenant-acme",
        type: "helm",
        status: "healthy",
        version: "6.7.1",
        lastSynced: new Date(Date.now() - 180_000).toISOString(),
      },
    ])

    let onEventCb:
      ((evt: import("../types/api").CrdWatchEvent) => void) | null = null
    vi.mocked(api.createEventSourceStream).mockImplementation((onEvent) => {
      onEventCb = onEvent
      return { close: vi.fn() } as unknown as EventSource
    })

    render(<TopologyView />)

    expect(await screen.findByText("14")).toBeInTheDocument()
    expect(screen.getByText("cloud")).toBeInTheDocument()
    expect(screen.getByText("2m ago")).toBeInTheDocument()
    expect(screen.getByText("3m ago")).toBeInTheDocument()

    await waitFor(() => expect(onEventCb).not.toBeNull())
    const dupEvent = {
      kind: "HelmRelease" as const,
      namespace: "tenant-acme",
      name: "podinfo",
      status: "Ready",
      message: "Reconciliation succeeded",
      revision: "6.7.1",
      timestamp: "2026-11-15T10:00:00Z",
    }
    onEventCb!(dupEvent)
    onEventCb!(dupEvent)

    const matches = await screen.findAllByText("Reconciliation succeeded")
    expect(matches).toHaveLength(1)
  })
})
