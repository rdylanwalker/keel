import { screen, fireEvent, waitFor, act, within } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import { TopologyView } from "./TopologyView"
import * as api from "../lib/api"
import { ApiError } from "../lib/errors"
import { buildAuthz } from "../lib/authz"
import { renderWithAuthz } from "../test/render"
import { CrdWatchEvent, ReleaseSummary, StackSummary } from "../types/api"

vi.mock("../lib/api", () => ({
  getStacksWithMeta: vi.fn(),
  getReleases: vi.fn(),
  reconcileStack: vi.fn(),
  reconcileRelease: vi.fn(),
  deleteStack: vi.fn(),
  deleteRelease: vi.fn(),
  createEventSourceStream: vi.fn(),
}))

vi.mock("./StackInspectorModal", () => ({
  StackInspectorModal: (p: {
    name: string
    kind?: string
    onClose: () => void
    onOpenSsh?: () => void
    onRestarted?: () => void
  }) => (
    <div role="dialog" aria-label={`inspector ${p.name} ${p.kind ?? "stack"}`}>
      <button onClick={p.onClose}>close inspector</button>
      <button onClick={p.onOpenSsh}>inspector ssh</button>
      <button onClick={p.onRestarted}>inspector restarted</button>
    </div>
  ),
}))

vi.mock("./VmSshModal", () => ({
  VmSshModal: (p: { stackName: string; onClose: () => void }) => (
    <div role="dialog" aria-label={`ssh ${p.stackName}`}>
      <button onClick={p.onClose}>close ssh</button>
    </div>
  ),
}))

const vmStack: StackSummary = {
  id: "s1",
  name: "live-proxmox-vm",
  namespace: "tenant-acme",
  tenant: "acme",
  provider: "proxmox",
  type: "workload",
  status: "Synced",
  resourceKind: "vm",
  resourceCount: 10,
  lastSynced: new Date(Date.now() - 120_000).toISOString(),
}
const clusterStack: StackSummary = {
  id: "s2",
  name: "aws-cluster",
  namespace: "tenant-acme",
  tenant: "acme",
  provider: "aws",
  type: "cloud",
  status: "Orphaned",
  resourceKind: "cluster",
}
const mgmtStack: StackSummary = {
  id: "m",
  name: "management-cluster-core",
  namespace: "flux-system",
  tenant: "management",
  provider: "kind",
  type: "management",
  status: "Synced",
  protect: true,
  resourceCount: 31,
  resourceKind: "cluster",
}
const release: ReleaseSummary = {
  id: "r1",
  name: "podinfo",
  tenant: "acme",
  namespace: "tenant-acme",
  type: "helm",
  status: "Reconciling",
  version: "6.7.1",
  chart: "podinfo",
}

type StreamHandlers = {
  onEvent: (e: CrdWatchEvent) => void
  onError?: (e: Event) => void
  es: { close: ReturnType<typeof vi.fn>; onopen: unknown }
}
let stream: StreamHandlers | null = null

function mockData(stacks: StackSummary[], releases: ReleaseSummary[] = []) {
  vi.mocked(api.getStacksWithMeta).mockResolvedValue({ stacks })
  vi.mocked(api.getReleases).mockResolvedValue(releases)
}

describe("TopologyView", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    window.history.replaceState(null, "", "/")
    stream = null
    vi.mocked(api.createEventSourceStream).mockImplementation(
      (onEvent, onError) => {
        const es = { close: vi.fn(), onopen: null }
        stream = { onEvent, onError, es }
        return es as unknown as EventSource
      }
    )
    mockData([vmStack, clusterStack], [release])
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it("renders live stacks into kind sections and never fabricates a management row", async () => {
    renderWithAuthz(<TopologyView />)
    expect(await screen.findByText("live-proxmox-vm")).toBeInTheDocument()
    expect(
      within(screen.getByTestId("topology-vms-section")).getByText(
        "live-proxmox-vm"
      )
    ).toBeInTheDocument()
    expect(
      within(screen.getByTestId("topology-clusters-section")).getByText(
        "aws-cluster"
      )
    ).toBeInTheDocument()
    expect(screen.queryByText("management-cluster-core")).toBeNull()
    expect(screen.queryByTestId("management-plane-strip")).toBeNull()
    expect(screen.getByText("Orphaned")).toBeInTheDocument()
    expect(screen.getByText("2m ago")).toBeInTheDocument()
  })

  it("renders the management strip only from real API data", async () => {
    mockData([mgmtStack])
    const onProviders = vi.fn()
    renderWithAuthz(<TopologyView onNavigateToProviders={onProviders} />)
    const strip = await screen.findByTestId("management-plane-strip")
    expect(within(strip).getByText("protect: true")).toBeInTheDocument()
    expect(within(strip).getByText("31 resources")).toBeInTheDocument()
    fireEvent.click(within(strip).getByRole("button", { name: /Providers/ }))
    expect(onProviders).toHaveBeenCalled()
    // protected management stack cannot be edited/deleted from the table
    expect(
      screen.getByRole("button", {
        name: "Delete stack management-cluster-core",
      })
    ).toBeDisabled()
  })

  it("keeps management stack empty on load failure and shows friendly error", async () => {
    vi.mocked(api.getStacksWithMeta).mockRejectedValue(
      new ApiError(500, "Internal server error", { correlationId: "abc-123" })
    )
    renderWithAuthz(<TopologyView />)
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Internal server error (reference: abc-123)"
    )
    expect(screen.queryByText("management-cluster-core")).toBeNull()
  })

  it("counts statuses including orphaned in the stat cards", async () => {
    renderWithAuthz(<TopologyView />)
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    const card = (label: string) =>
      screen.getByText(label, { selector: "p" }).parentElement!.textContent
    expect(card("Total Stacks & Apps")).toContain("3")
    expect(card("Synced & Healthy")).toContain("1")
    expect(card("Reconciling")).toContain("1")
    expect(card("Drifted / Error / Deleting / Orphaned")).toContain("1")
  })

  it("navigates to the catalog pre-filtered by kind", async () => {
    const onCatalog = vi.fn()
    renderWithAuthz(<TopologyView onNavigateToCatalog={onCatalog} />)
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    fireEvent.click(screen.getByRole("button", { name: "+ Deploy Cluster" }))
    fireEvent.click(screen.getByRole("button", { name: "+ Deploy VM" }))
    fireEvent.click(screen.getByRole("button", { name: "+ Deploy Service" }))
    fireEvent.click(screen.getByRole("button", { name: "+ Deploy App" }))
    expect(onCatalog.mock.calls.map((c) => c[0])).toEqual([
      "cluster",
      "vm",
      "service",
      "app",
    ])
  })

  it("runs reconcile/delete/edit/inspect/ssh row actions", async () => {
    vi.mocked(api.reconcileStack).mockResolvedValue({
      reconciled: true,
      namespace: "tenant-acme",
      name: "live-proxmox-vm",
      message: "Stack reconcile requested",
    })
    vi.mocked(api.reconcileRelease).mockResolvedValue({
      reconciled: true,
      namespace: "tenant-acme",
      name: "podinfo",
      message: "Release reconcile requested",
    })
    const proposal = {
      id: "901",
      title: "t",
      branch: "b",
      author: "a",
      tenant: "acme",
      status: "open" as const,
      createdAt: "now",
    }
    vi.mocked(api.deleteStack).mockResolvedValue({
      id: "x",
      namespace: "tenant-acme",
      branch: "del-branch",
      proposal,
    })
    vi.mocked(api.deleteRelease).mockResolvedValue({
      id: "y",
      namespace: "tenant-acme",
      branch: "del-rel",
      proposal: { ...proposal, id: "902" },
    })
    const onEditStack = vi.fn()
    const onEditRelease = vi.fn()
    renderWithAuthz(
      <TopologyView onEditStack={onEditStack} onEditRelease={onEditRelease} />
    )
    await screen.findByRole("button", { name: "Inspect release podinfo" })

    fireEvent.click(
      screen.getByRole("button", { name: "Edit stack live-proxmox-vm" })
    )
    expect(onEditStack).toHaveBeenCalledWith("tenant-acme", "live-proxmox-vm")
    fireEvent.click(
      screen.getByRole("button", { name: "Edit release podinfo" })
    )
    expect(onEditRelease).toHaveBeenCalledWith("tenant-acme", "podinfo")

    fireEvent.click(
      screen.getByRole("button", { name: "Reconcile stack live-proxmox-vm" })
    )
    expect(
      await screen.findByText("Stack reconcile requested")
    ).toBeInTheDocument()
    fireEvent.click(
      screen.getByRole("button", { name: "Reconcile release podinfo" })
    )
    expect(
      await screen.findByText("Release reconcile requested")
    ).toBeInTheDocument()

    fireEvent.click(
      screen.getByRole("button", { name: "Delete stack live-proxmox-vm" })
    )
    expect(
      await screen.findByText(/Opened Delete PR #901.*live-proxmox-vm/)
    ).toBeInTheDocument()
    fireEvent.click(
      screen.getByRole("button", { name: "Delete release podinfo" })
    )
    expect(
      await screen.findByText(/Opened Delete PR #902.*podinfo/)
    ).toBeInTheDocument()

    fireEvent.click(
      screen.getByRole("button", { name: "Inspect release podinfo" })
    )
    expect(
      screen.getByRole("dialog", { name: "inspector podinfo release" })
    ).toBeInTheDocument()
    fireEvent.click(screen.getByText("inspector ssh"))
    expect(
      await screen.findByRole("dialog", { name: "ssh podinfo" })
    ).toBeInTheDocument()
    fireEvent.click(screen.getByText("close ssh"))
    fireEvent.click(screen.getByText("inspector restarted"))
    fireEvent.click(screen.getByText("close inspector"))

    fireEvent.click(screen.getByTestId("stack-ssh-btn-live-proxmox-vm"))
    expect(
      await screen.findByRole("dialog", { name: "ssh live-proxmox-vm" })
    ).toBeInTheDocument()
  })

  it("surfaces action errors (403/409) in the alert", async () => {
    vi.mocked(api.reconcileStack).mockRejectedValue(
      new ApiError(403, "Access denied for tenant 'acme'")
    )
    vi.mocked(api.deleteRelease).mockRejectedValue(
      new ApiError(409, "Stack is protected")
    )
    vi.mocked(api.reconcileRelease).mockRejectedValue("boom")
    vi.mocked(api.deleteStack).mockRejectedValue(new Error("nope"))
    renderWithAuthz(<TopologyView />)
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    fireEvent.click(
      screen.getByRole("button", { name: "Reconcile stack live-proxmox-vm" })
    )
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Access denied for tenant 'acme'"
    )
    fireEvent.click(
      screen.getByRole("button", { name: "Delete release podinfo" })
    )
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent("Stack is protected")
    )
    fireEvent.click(
      screen.getByRole("button", { name: "Reconcile release podinfo" })
    )
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to reconcile release"
      )
    )
    fireEvent.click(
      screen.getByRole("button", { name: "Delete stack live-proxmox-vm" })
    )
    await waitFor(() =>
      expect(screen.getByRole("alert")).toHaveTextContent("nope")
    )
  })

  it("disables row mutations for users without tenant access", async () => {
    const member = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["other"],
    })
    renderWithAuthz(
      <TopologyView onEditStack={vi.fn()} onEditRelease={vi.fn()} />,
      member
    )
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    for (const name of [
      "Edit stack live-proxmox-vm",
      "Reconcile stack live-proxmox-vm",
      "Delete stack live-proxmox-vm",
      "Edit release podinfo",
      "Reconcile release podinfo",
      "Delete release podinfo",
    ]) {
      expect(screen.getByRole("button", { name })).toBeDisabled()
    }
    expect(
      screen.getByRole("button", { name: "Inspect release podinfo" })
    ).toBeEnabled()
  })

  it("uses SSE instead of polling while connected and falls back to backoff polling", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    renderWithAuthz(<TopologyView />)
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    expect(api.getStacksWithMeta).toHaveBeenCalledTimes(1)
    expect(screen.getByTestId("topology-stream-status")).toHaveTextContent(
      "Connecting"
    )

    act(() => {
      stream!.onEvent({
        kind: "Stack",
        namespace: "tenant-acme",
        name: "live-proxmox-vm",
        status: "Ready",
        message: "Stack synced",
        timestamp: new Date().toISOString(),
      })
    })
    expect(screen.getByTestId("topology-stream-status")).toHaveTextContent(
      "SSE Live Stream"
    )
    expect(screen.getByText("Stack synced")).toBeInTheDocument()
    // No interval polling while live: only the debounced refresh after the event.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000)
    })
    expect(api.getStacksWithMeta).toHaveBeenCalledTimes(2)

    act(() => stream!.onError?.(new Event("error")))
    expect(screen.getByTestId("topology-stream-status")).toHaveTextContent(
      "SSE disconnected · polling 5s"
    )
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000)
    })
    expect(api.getStacksWithMeta).toHaveBeenCalledTimes(3)
    expect(screen.getByTestId("topology-stream-status")).toHaveTextContent(
      "polling 10s"
    )
  })

  it("deduplicates SSE events, filters, pauses and clears the stream", async () => {
    renderWithAuthz(<TopologyView />)
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    const ev: CrdWatchEvent = {
      kind: "HelmRelease",
      namespace: "tenant-acme",
      name: "podinfo",
      status: "Ready",
      message: "Release ready",
      timestamp: "2026-01-01T00:00:00Z",
    }
    act(() => {
      stream!.onEvent(ev)
      stream!.onEvent(ev)
    })
    expect(screen.getAllByText("Release ready")).toHaveLength(1)
    fireEvent.change(screen.getByLabelText(/Filter events by CRD kind/i), {
      target: { value: "Stack" },
    })
    expect(screen.queryByText("Release ready")).toBeNull()
    fireEvent.click(screen.getByRole("button", { name: /Pause Stream/i }))
    expect(stream!.es.close).toHaveBeenCalled()
    expect(screen.getByTestId("topology-stream-status")).toHaveTextContent(
      "SSE Paused"
    )
    fireEvent.click(screen.getByRole("button", { name: /Clear/i }))
  })

  it("refresh button refetches and reports errors", async () => {
    renderWithAuthz(<TopologyView />)
    await screen.findByRole("button", { name: "Inspect release podinfo" })
    vi.mocked(api.getStacksWithMeta).mockRejectedValueOnce(
      new Error("Refresh failed")
    )
    fireEvent.click(screen.getByRole("button", { name: /Refresh Map/i }))
    expect(await screen.findByRole("alert")).toHaveTextContent("Refresh failed")
  })

  it("renders demo fixtures without any live API calls", async () => {
    window.history.replaceState(null, "", "/?demo=true")
    renderWithAuthz(<TopologyView />)
    expect(screen.getByText("management-cluster-core")).toBeInTheDocument()
    expect(screen.getByTestId("topology-stream-status")).toHaveTextContent(
      "Demo data"
    )
    fireEvent.click(screen.getByRole("button", { name: /Refresh Map/i }))
    await act(async () => {})
    expect(api.getStacksWithMeta).not.toHaveBeenCalled()
    expect(api.createEventSourceStream).not.toHaveBeenCalled()
  })

  it("promotes active canary rollouts", async () => {
    mockData(
      [],
      [
        {
          ...release,
          canary: true,
          canaryWeight: 35,
          canaryPhase: "Progressing",
          primaryVersion: "6.7.0",
          errorRate: 0.01,
          latencyMs: 14,
        },
      ]
    )
    renderWithAuthz(<TopologyView />)
    expect(await screen.findByText("Canary 35%")).toBeInTheDocument()
    expect(screen.getByText("6.7.0 → 6.7.1")).toBeInTheDocument()
    expect(
      screen.queryByText("No active canary traffic rollouts found.")
    ).toBeNull()
  })

  it("does not load when unauthenticated", () => {
    renderWithAuthz(<TopologyView authenticated={false} />)
    expect(api.getStacksWithMeta).not.toHaveBeenCalled()
  })
})
