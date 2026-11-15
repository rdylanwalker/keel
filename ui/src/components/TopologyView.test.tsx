import { render, screen, fireEvent, waitFor, act } from "@testing-library/react"
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
  restartStackWorkspace: vi.fn(),
  reconcileRelease: vi.fn(),
  deleteStack: vi.fn(),
  deleteRelease: vi.fn(),
  getStackDiagnostics: vi.fn().mockResolvedValue({
    name: "live-proxmox-stack",
    namespace: "tenant-acme",
    kind: "stack",
    status: "Ready",
    conditions: [],
    events: [],
    logs: [],
    outputs: {},
  }),
  getReleaseDiagnostics: vi.fn().mockResolvedValue({
    name: "podinfo",
    namespace: "tenant-acme",
    kind: "release",
    status: "Ready",
    conditions: [],
    events: [],
    logs: [],
    outputs: {},
  }),
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

  it("renders cluster & workload topology and loads live stacks", async () => {
    vi.mocked(api.getStacks).mockResolvedValueOnce(mockLiveStacks)

    render(<TopologyView />)

    expect(screen.getByText("Cluster & Workload Topology")).toBeInTheDocument()
    expect(screen.getByText("Management Control Plane")).toBeInTheDocument()

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
      expect(screen.getByText("live-aws-stack")).toBeInTheDocument()
      // Always includes management-cluster-core by default
      expect(screen.getByText("management-cluster-core")).toBeInTheDocument()
    })
  })

  it("expands compact control plane strip details and navigates to providers tab", async () => {
    const onNavigateToProviders = vi.fn()
    vi.mocked(api.getStacksWithMeta).mockResolvedValueOnce({
      stacks: mockLiveStacks,
      gitRepoUrl: "https://git.example.com/org/infra.git",
      forgeProvider: "forgejo",
      stateBackend: "s3://keel-state",
    })

    render(<TopologyView onNavigateToProviders={onNavigateToProviders} />)

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
    })

    expect(screen.queryByText("Git Forge & Repo")).not.toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: /Details/i }))
    expect(screen.getByText("Git Forge & Repo")).toBeInTheDocument()
    expect(
      screen.getByText("forgejo: https://git.example.com/org/infra.git")
    ).toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: /Hide/i }))
    expect(screen.queryByText("Git Forge & Repo")).not.toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: /Providers →/i }))
    expect(onNavigateToProviders).toHaveBeenCalled()
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
    act(() => {
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
    act(() => {
      onEventCb!(dupEvent)
      onEventCb!(dupEvent)
    })

    const matches = await screen.findAllByText("Reconciliation succeeded")
    expect(matches).toHaveLength(1)
  })

  it("handles stack and release quick actions (edit, inspect, restart, reconcile, delete)", async () => {
    const onEditStack = vi.fn()
    const onEditRelease = vi.fn()
    const onNavigateToCatalog = vi.fn()

    vi.mocked(api.getStacksWithMeta).mockResolvedValue({
      stacks: mockLiveStacks,
    })
    vi.mocked(api.getReleases).mockResolvedValue([
      {
        id: "tenant-acme/podinfo",
        name: "podinfo",
        namespace: "tenant-acme",
        tenant: "acme",
        chart: "podinfo",
        version: "6.7.0",
        type: "helm",
        status: "Ready",
        lastSynced: new Date(Date.now() - 120000).toISOString(),
        targetCluster: "workload-01-kubeconfig",
      },
    ])
    vi.mocked(api.restartStackWorkspace).mockResolvedValueOnce({
      restarted: true,
      namespace: "tenant-acme",
      name: "live-proxmox-stack",
      message: "Workspace restarted for live-proxmox-stack",
    })
    vi.mocked(api.reconcileRelease).mockResolvedValueOnce({
      reconciled: true,
      namespace: "tenant-acme",
      name: "podinfo",
      message: "Reconciliation requested for podinfo",
    })
    vi.mocked(api.deleteStack).mockResolvedValueOnce({
      id: "live-proxmox-stack",
      namespace: "tenant-acme",
      branch: "keel/delete/live-proxmox-stack",
      proposal: {
        id: "901",
        title: "Delete stack",
        branch: "keel/delete/live-proxmox-stack",
        author: "user",
        tenant: "tenant-acme",
        status: "open",
        createdAt: "now",
      },
    })
    vi.mocked(api.deleteRelease).mockResolvedValueOnce({
      id: "podinfo",
      namespace: "tenant-acme",
      branch: "keel/delete/podinfo",
      proposal: {
        id: "902",
        title: "Delete release",
        branch: "keel/delete/podinfo",
        author: "user",
        tenant: "tenant-acme",
        status: "open",
        createdAt: "now",
      },
    })

    render(
      <TopologyView
        onEditStack={onEditStack}
        onEditRelease={onEditRelease}
        onNavigateToCatalog={onNavigateToCatalog}
      />
    )

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
      expect(screen.getAllByText("podinfo").length).toBeGreaterThan(0)
    })

    // Edit stack & release
    fireEvent.click(
      screen.getByRole("button", { name: "Edit stack live-proxmox-stack" })
    )
    expect(onEditStack).toHaveBeenCalledWith(
      "tenant-acme",
      "live-proxmox-stack"
    )

    fireEvent.click(
      screen.getByRole("button", { name: "Edit release podinfo" })
    )
    expect(onEditRelease).toHaveBeenCalledWith("tenant-acme", "podinfo")

    // Restart stack
    fireEvent.click(
      screen.getByRole("button", { name: "Restart stack live-proxmox-stack" })
    )
    await waitFor(() => {
      expect(
        screen.getByText("Workspace restarted for live-proxmox-stack")
      ).toBeInTheDocument()
    })

    // Reconcile release
    fireEvent.click(
      screen.getByRole("button", { name: "Reconcile release podinfo" })
    )
    await waitFor(() => {
      expect(
        screen.getByText("Reconciliation requested for podinfo")
      ).toBeInTheDocument()
    })

    // Delete stack & release PR banners
    fireEvent.click(
      screen.getByRole("button", { name: "Delete stack live-proxmox-stack" })
    )
    await waitFor(() => {
      expect(
        screen.getByText(/Opened Delete PR #901.*live-proxmox-stack/)
      ).toBeInTheDocument()
    })

    fireEvent.click(
      screen.getByRole("button", { name: "Delete release podinfo" })
    )
    await waitFor(() => {
      expect(
        screen.getByText(/Opened Delete PR #902.*podinfo/)
      ).toBeInTheDocument()
    })

    // Dismiss banners
    const dismissButtons = screen.getAllByRole("button", { name: /Dismiss/i })
    dismissButtons.forEach((btn) => fireEvent.click(btn))

    // Toggle Auto refresh & + Deploy buttons
    fireEvent.click(screen.getByRole("button", { name: "Auto: 5s" }))
    expect(
      screen.getByRole("button", { name: "Auto: Off" })
    ).toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: "+ Deploy Stack" }))
    fireEvent.click(screen.getByRole("button", { name: "+ Deploy App" }))
    expect(onNavigateToCatalog).toHaveBeenCalledTimes(2)

    // Inspect stack modal open & close
    fireEvent.click(
      screen.getByRole("button", { name: "Inspect stack live-proxmox-stack" })
    )
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeInTheDocument()
    })
  })

  it("handles errors on restart, reconcile, delete, and clears event stream", async () => {
    vi.mocked(api.getStacksWithMeta).mockResolvedValue({
      stacks: mockLiveStacks,
    })
    vi.mocked(api.getReleases).mockResolvedValue([
      {
        id: "tenant-acme/podinfo",
        name: "podinfo",
        namespace: "tenant-acme",
        tenant: "acme",
        chart: "podinfo",
        version: "6.7.0",
        type: "helm",
        status: "Ready",
        lastSynced: new Date(Date.now() - 3 * 3600 * 1000).toISOString(),
      },
      {
        id: "tenant-acme/redis",
        name: "redis",
        namespace: "tenant-acme",
        tenant: "acme",
        chart: "redis",
        version: "1.0.0",
        type: "helm",
        status: "Ready",
        lastSynced: new Date(Date.now() - 2 * 86400 * 1000).toISOString(),
      },
    ])
    vi.mocked(api.restartStackWorkspace).mockRejectedValueOnce(
      new Error("Restart failed")
    )
    vi.mocked(api.reconcileRelease).mockRejectedValueOnce(
      new Error("Reconcile failed")
    )
    vi.mocked(api.deleteStack).mockRejectedValueOnce(
      new Error("Delete stack failed")
    )
    vi.mocked(api.deleteRelease).mockRejectedValueOnce(
      new Error("Delete release failed")
    )

    render(<TopologyView />)

    await waitFor(() => {
      expect(screen.getByText("live-proxmox-stack")).toBeInTheDocument()
      expect(screen.getAllByText("podinfo").length).toBeGreaterThan(0)
    })

    fireEvent.click(
      screen.getByRole("button", { name: "Restart stack live-proxmox-stack" })
    )
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Restart failed")
    })

    fireEvent.click(
      screen.getByRole("button", { name: "Reconcile release podinfo" })
    )
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Reconcile failed")
    })

    fireEvent.click(
      screen.getByRole("button", { name: "Delete stack live-proxmox-stack" })
    )
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Delete stack failed")
    })

    fireEvent.click(
      screen.getByRole("button", { name: "Delete release podinfo" })
    )
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Delete release failed"
      )
    })

    // Clear event logs
    fireEvent.click(screen.getByTitle("Clear event logs"))
    expect(
      screen.getByText("No CRD events received yet or stream cleared.")
    ).toBeInTheDocument()
  })
})
