import { screen, fireEvent, waitFor, act, within } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import { TopologyView } from "./TopologyView"
import * as api from "../lib/api"
import { ApiError } from "../lib/errors"
import { buildAuthz } from "../lib/authz"
import { renderWithAuthz } from "../test/render"
import {
  CatalogItem,
  CrdWatchEvent,
  ReleaseSummary,
  StackSummary,
} from "../types/api"

vi.mock("../lib/api", () => ({
  getStacksWithMeta: vi.fn(),
  getReleases: vi.fn(),
  reconcileStack: vi.fn(),
  reconcileRelease: vi.fn(),
  deleteStack: vi.fn(),
  deleteRelease: vi.fn(),
  createEventSourceStream: vi.fn(),
  getCoreAddons: vi.fn(),
  rollbackManagementAddon: vi.fn(),
  upgradeManagementAddon: vi.fn(),
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

function addonRelease(
  name: string,
  extra: Partial<ReleaseSummary> = {}
): ReleaseSummary {
  return {
    name,
    namespace: "flux-system",
    tenant: "management",
    type: "helm",
    runtime: "helm",
    chart: name,
    version: "1.0.0",
    desiredVersion: "1.0.0",
    status: "Ready",
    managementAddon: true,
    protected: true,
    catalogId: name,
    addonKind: "HelmRelease",
    ...extra,
  }
}

function coreAddon(id: string, extra: Partial<CatalogItem> = {}): CatalogItem {
  return {
    id,
    name: id,
    description: `${id} addon`,
    type: "app",
    category: "Management Addons",
    chart: id,
    version: "1.0.0",
    schema: { type: "object", properties: {} },
    ...extra,
  }
}

const TENANT_MEMBER = buildAuthz({
  authenticated: true,
  phase: "oidc-active",
  groups: ["tenant-acme"],
  isAdmin: false,
})

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
    vi.mocked(api.getCoreAddons).mockResolvedValue([])
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
    expect(api.getCoreAddons).not.toHaveBeenCalled()
  })
  describe("Management add-on rows in the releases table", () => {
    const addons = [
      addonRelease("kyverno", {
        desiredVersion: "3.4.0",
        version: "3.4.0",
        lastSynced: new Date(Date.now() - 60_000).toISOString(),
      }),
      addonRelease("rauthy", {
        addonKind: "Kustomization",
        runtime: "kustomize",
        type: "kustomize",
        desiredVersion: "0.37.2",
        status: "Reconciling",
      }),
      addonRelease("forgejo", { conflict: true, status: "Pending" }),
      addonRelease("orphan-addon", { status: "Error" }),
    ]
    const releasesSection = () =>
      screen.getByTestId("topology-releases-section")
    const addonRow = (id: string) =>
      screen.getByTestId(`management-addon-row-${id}`)
    const rowNames = () =>
      within(releasesSection())
        .getAllByRole("row")
        .slice(1)
        .map((r) => r.getAttribute("data-testid") ?? "tenant")

    it("lists protected add-ons first in the single releases table with status, version, kind and conflict", async () => {
      mockData([], [release, ...addons])
      vi.mocked(api.getCoreAddons).mockResolvedValue([
        coreAddon("kyverno", { name: "Kyverno" }),
        coreAddon("rauthy", {
          name: "Rauthy OIDC Provider",
          serviceUrl: "https://idp.keel.local:8888/auth/v1/admin",
        }),
        coreAddon("forgejo"),
      ])
      renderWithAuthz(<TopologyView onEditRelease={vi.fn()} />)

      await screen.findByTestId("management-addon-row-kyverno")
      // No separate card grid: one table, management rows first.
      expect(screen.queryByTestId("management-addons-group")).toBeNull()
      expect(within(releasesSection()).getAllByRole("table")).toHaveLength(1)
      expect(rowNames()).toEqual([
        "management-addon-row-kyverno",
        "management-addon-row-rauthy",
        "management-addon-row-forgejo",
        "management-addon-row-orphan-addon",
        "tenant",
      ])

      const kyverno = addonRow("kyverno")
      expect(within(kyverno).getByText("Protected")).toBeInTheDocument()
      expect(within(kyverno).getByText("3.4.0")).toBeInTheDocument()
      expect(within(kyverno).getByText("1m ago")).toBeInTheDocument()
      expect(within(kyverno).getByText("HelmRelease")).toBeInTheDocument()
      expect(within(kyverno).getByText("flux-system")).toBeInTheDocument()
      expect(
        screen.getByTestId("management-addon-status-kyverno")
      ).toHaveTextContent("Ready")
      expect(
        screen.getByTestId("management-addon-status-rauthy")
      ).toHaveTextContent("Reconciling")
      expect(
        screen.getByTestId("management-addon-status-orphan-addon")
      ).toHaveTextContent("Error")
      expect(within(addonRow("rauthy")).getByText("0.37.2")).toBeInTheDocument()
      // Catalog display name is a visible secondary line; description is screen-reader text.
      expect(
        await within(addonRow("rauthy")).findByText("Rauthy OIDC Provider")
      ).toBeInTheDocument()
      expect(within(addonRow("rauthy")).getByText("rauthy addon")).toHaveClass(
        "sr-only"
      )
      expect(
        within(addonRow("rauthy")).getByText("Kustomization")
      ).toBeInTheDocument()
      expect(
        await within(addonRow("rauthy")).findByRole("link", {
          name: "https://idp.keel.local:8888/auth/v1/admin",
        })
      ).toHaveAttribute("href", "https://idp.keel.local:8888/auth/v1/admin")

      // Conflicting definitions disable PR actions.
      expect(
        screen.getByTestId("management-addon-conflict-forgejo")
      ).toBeInTheDocument()
      await waitFor(() =>
        expect(
          screen.getByRole("button", { name: "Upgrade kyverno" })
        ).toBeEnabled()
      )
      expect(
        screen.getByRole("button", { name: "Upgrade forgejo" })
      ).toBeDisabled()
      expect(
        screen.getByRole("button", { name: "Rollback forgejo" })
      ).toBeDisabled()
      // No catalog/core entry: upgrade unavailable, rollback still possible.
      expect(
        screen.getByRole("button", { name: "Upgrade orphan-addon" })
      ).toBeDisabled()
      expect(
        screen.getByRole("button", { name: "Rollback orphan-addon" })
      ).toBeEnabled()

      // Management rows: Diagnostics / Reconcile / Upgrade / Rollback, never Edit or Delete.
      for (const a of addons) {
        const id = a.name
        const row = addonRow(id)
        expect(
          within(row)
            .getAllByRole("button")
            .map((b) => b.getAttribute("aria-label"))
        ).toEqual([
          `Diagnostics for ${id}`,
          `Reconcile ${id}`,
          `Upgrade ${id}`,
          `Rollback ${id}`,
        ])
      }
      // Tenant rows keep their existing actions.
      const tenantRow = within(releasesSection()).getAllByRole("row").at(-1)!
      expect(
        within(tenantRow)
          .getAllByRole("button")
          .map((b) => b.getAttribute("aria-label"))
      ).toEqual([
        "Edit release podinfo",
        "Inspect release podinfo",
        "Reconcile release podinfo",
        "Delete release podinfo",
      ])
    })

    it("finds add-on rows by filtering for management or protected", async () => {
      mockData([], [release, addons[0]])
      vi.mocked(api.getCoreAddons).mockResolvedValue([
        coreAddon("kyverno", { name: "Kyverno Policy Engine" }),
      ])
      renderWithAuthz(<TopologyView />)
      await screen.findByTestId("management-addon-row-kyverno")
      const filter = within(releasesSection()).getByRole("textbox")
      for (const q of ["management", "Protected", "Policy Engine"]) {
        fireEvent.change(filter, { target: { value: q } })
        await waitFor(() =>
          expect(rowNames()).toEqual(["management-addon-row-kyverno"])
        )
      }
      fireEvent.change(filter, { target: { value: "podinfo" } })
      await waitFor(() => expect(rowNames()).toEqual(["tenant"]))
    })

    it("renders both Flux objects of a conflicting add-on as distinct rows", async () => {
      mockData(
        [],
        [
          addonRelease("forgejo", { conflict: true }),
          addonRelease("forgejo", {
            conflict: true,
            addonKind: "Kustomization",
            runtime: "kustomize",
            type: "kustomize",
          }),
        ]
      )
      vi.mocked(api.getCoreAddons).mockResolvedValue([coreAddon("forgejo")])
      const errors = vi.spyOn(console, "error").mockImplementation(() => {})
      renderWithAuthz(<TopologyView />)
      await screen.findAllByTestId("management-addon-row-forgejo")
      const rows = screen.getAllByTestId("management-addon-row-forgejo")
      expect(rows).toHaveLength(2)
      expect(
        screen
          .getAllByTestId("management-addon-kind-forgejo")
          .map((k) => k.textContent)
      ).toEqual(["HelmRelease", "Kustomization"])
      expect(
        screen.getAllByTestId("management-addon-conflict-forgejo")
      ).toHaveLength(2)
      for (const row of rows) {
        expect(
          within(row).getByRole("button", { name: "Upgrade forgejo" })
        ).toBeDisabled()
        expect(
          within(row).getByRole("button", { name: "Rollback forgejo" })
        ).toBeDisabled()
      }
      expect(
        errors.mock.calls.some((c) => String(c[0]).includes("same key"))
      ).toBe(false)
      errors.mockRestore()
    })

    it("sorts management and tenant rows together by plain release name", async () => {
      mockData(
        [],
        [
          release,
          addonRelease("rauthy", { addonKind: "Kustomization" }),
          addonRelease("kyverno"),
          addonRelease("cert-manager"),
        ]
      )
      vi.mocked(api.getCoreAddons).mockResolvedValue([
        coreAddon("rauthy", { name: "Rauthy OIDC Provider" }),
        coreAddon("kyverno", { name: "Kyverno" }),
      ])
      renderWithAuthz(<TopologyView />)
      await screen.findByTestId("management-addon-row-kyverno")
      expect(rowNames()).toEqual([
        "management-addon-row-rauthy",
        "management-addon-row-kyverno",
        "management-addon-row-cert-manager",
        "tenant",
      ])
      fireEvent.click(within(releasesSection()).getByText("Release Name"))
      await waitFor(() =>
        expect(rowNames()).toEqual([
          "management-addon-row-cert-manager",
          "management-addon-row-kyverno",
          "tenant",
          "management-addon-row-rauthy",
        ])
      )
    })

    it("runs diagnostics and reconcile against the add-on Flux object", async () => {
      mockData([], [addons[0]])
      vi.mocked(api.reconcileRelease).mockResolvedValue({
        reconciled: true,
        namespace: "flux-system",
        name: "kyverno",
        message: "Reconcile requested for flux-system/kyverno",
      })
      renderWithAuthz(<TopologyView />)
      fireEvent.click(
        await screen.findByTestId("management-addon-diagnostics-kyverno")
      )
      expect(
        screen.getByRole("dialog", { name: "inspector kyverno release" })
      ).toBeInTheDocument()
      fireEvent.click(screen.getByText("close inspector"))

      fireEvent.click(screen.getByTestId("management-addon-reconcile-kyverno"))
      await waitFor(() => {
        expect(api.reconcileRelease).toHaveBeenCalledWith(
          "flux-system",
          "kyverno"
        )
        expect(
          screen.getByText("Reconcile requested for flux-system/kyverno")
        ).toBeInTheDocument()
      })
    })

    it("submits the catalog/core upgrade form and confirmed rollback PRs", async () => {
      mockData([], [addons[0]])
      vi.mocked(api.getCoreAddons).mockResolvedValue([
        coreAddon("kyverno", { name: "Kyverno", version: "3.4.0" }),
      ])
      vi.mocked(api.upgradeManagementAddon).mockResolvedValueOnce({
        id: "500",
        title: "Upgrade kyverno",
        branch: "keel/mgmt/upgrade-kyverno",
        author: "admin",
        tenant: "keel-system",
        status: "open",
        createdAt: "now",
        prUrl: "https://forge.example/pr/500",
      })
      vi.mocked(api.rollbackManagementAddon)
        .mockResolvedValueOnce({
          id: "501",
          title: "Rollback kyverno",
          branch: "keel/mgmt/rollback-kyverno",
          author: "admin",
          tenant: "keel-system",
          status: "open",
          createdAt: "now",
          prUrl: "https://forge.example/pr/501",
        })
        .mockRejectedValueOnce(
          new ApiError(400, "No previous revision found to roll back to")
        )
      renderWithAuthz(<TopologyView />)

      const upgrade = await screen.findByTestId(
        "management-addon-upgrade-kyverno"
      )
      await waitFor(() => expect(upgrade).toBeEnabled())
      fireEvent.click(upgrade)
      expect(
        screen.getByRole("dialog", { name: /Upgrade Kyverno/i })
      ).toBeInTheDocument()
      fireEvent.change(screen.getByLabelText(/Chart Version/i), {
        target: { value: "3.5.0" },
      })
      fireEvent.click(
        screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
      )
      await waitFor(() => {
        expect(api.upgradeManagementAddon).toHaveBeenCalledWith("kyverno", {
          version: "3.5.0",
          values: {},
        })
        expect(
          screen.getByText(
            "Opened Upgrade PR #500 (keel/mgmt/upgrade-kyverno) for management add-on kyverno"
          )
        ).toBeInTheDocument()
      })
      expect(
        screen.queryByRole("dialog", { name: /Upgrade Kyverno/i })
      ).toBeNull()
      expect(
        screen.getByRole("link", { name: "View on Forge" })
      ).toHaveAttribute("href", "https://forge.example/pr/500")

      const rollback = screen.getByTestId("management-addon-rollback-kyverno")
      fireEvent.click(rollback)
      fireEvent.click(await screen.findByRole("button", { name: "Cancel" }))
      expect(api.rollbackManagementAddon).not.toHaveBeenCalled()

      fireEvent.click(rollback)
      expect(
        await screen.findByRole("alertdialog", { name: /Roll back kyverno/i })
      ).toBeInTheDocument()
      fireEvent.click(screen.getByRole("button", { name: "Open Rollback PR" }))
      await waitFor(() => {
        expect(api.rollbackManagementAddon).toHaveBeenCalledWith("kyverno")
        expect(
          screen.getByText(
            "Opened Rollback PR #501 (keel/mgmt/rollback-kyverno) for management add-on kyverno"
          )
        ).toBeInTheDocument()
      })
      expect(
        screen.getByRole("link", { name: "View on Forge" })
      ).toHaveAttribute("href", "https://forge.example/pr/501")

      // The Rollback button stays mounted through the rollback, so dialog focus can return to it.
      expect(screen.getByTestId("management-addon-rollback-kyverno")).toBe(
        rollback
      )
      fireEvent.click(rollback)
      fireEvent.click(
        await screen.findByRole("button", { name: "Open Rollback PR" })
      )
      expect(await screen.findByRole("alert")).toHaveTextContent(
        "No previous revision found to roll back to"
      )
    })

    it("keeps the upgrade form open when a releases refresh fails", async () => {
      mockData([], [addons[0]])
      vi.mocked(api.getCoreAddons).mockResolvedValue([
        coreAddon("kyverno", { name: "Kyverno" }),
      ])
      renderWithAuthz(<TopologyView />)
      const upgrade = await screen.findByTestId(
        "management-addon-upgrade-kyverno"
      )
      await waitFor(() => expect(upgrade).toBeEnabled())
      fireEvent.click(upgrade)
      fireEvent.change(screen.getByLabelText(/Chart Version/i), {
        target: { value: "9.9.9" },
      })

      vi.mocked(api.getReleases).mockRejectedValue(new Error("boom"))
      // The modal hides the page from the a11y tree; click the refresh control directly.
      fireEvent.click(screen.getByText("Refresh Map"))
      await waitFor(() =>
        expect(screen.queryByTestId("management-addon-row-kyverno")).toBeNull()
      )
      expect(
        screen.getByRole("dialog", { name: /Upgrade Kyverno/i })
      ).toBeInTheDocument()
      expect(screen.getByLabelText(/Chart Version/i)).toHaveValue("9.9.9")
      expect(api.getCoreAddons).toHaveBeenCalledTimes(1)
    })

    it("keeps the rollback confirmation open when a refresh returns no add-ons", async () => {
      mockData([], [addons[0]])
      vi.mocked(api.rollbackManagementAddon).mockResolvedValueOnce({
        id: "502",
        title: "Rollback kyverno",
        branch: "keel/mgmt/rollback-kyverno",
        author: "admin",
        tenant: "keel-system",
        status: "open",
        createdAt: "now",
      })
      renderWithAuthz(<TopologyView />)
      fireEvent.click(
        await screen.findByTestId("management-addon-rollback-kyverno")
      )
      await screen.findByRole("alertdialog", { name: /Roll back kyverno/i })

      vi.mocked(api.getReleases).mockResolvedValue([])
      fireEvent.click(screen.getByText("Refresh Map"))
      await waitFor(() =>
        expect(screen.queryByTestId("management-addon-row-kyverno")).toBeNull()
      )
      fireEvent.click(
        screen.getByRole("button", { name: "Open Rollback PR", hidden: true })
      )
      await waitFor(() => {
        expect(api.rollbackManagementAddon).toHaveBeenCalledWith("kyverno")
        expect(
          screen.getByText(
            "Opened Rollback PR #502 (keel/mgmt/rollback-kyverno) for management add-on kyverno"
          )
        ).toBeInTheDocument()
      })
    })

    it("lists add-ons for an admin scoped to a tenant via ?tenant=management", async () => {
      vi.mocked(api.getStacksWithMeta).mockResolvedValue({ stacks: [] })
      vi.mocked(api.getReleases).mockImplementation(async (tenant) =>
        tenant === "management" ? [addons[0]] : [release]
      )
      renderWithAuthz(<TopologyView selectedTenant="acme" />)
      await screen.findByTestId("management-addon-row-kyverno")
      expect(rowNames()).toEqual(["management-addon-row-kyverno", "tenant"])
      expect(api.getReleases).toHaveBeenCalledWith("acme")
      expect(api.getReleases).toHaveBeenCalledWith("management")
      expect(
        screen.getByRole("button", { name: "Delete release podinfo" })
      ).toBeInTheDocument()
    })

    it("does not request management add-ons for non-admins scoped to a tenant", async () => {
      renderWithAuthz(<TopologyView selectedTenant="acme" />, TENANT_MEMBER)
      await screen.findByRole("button", { name: "Inspect release podinfo" })
      expect(api.getReleases).toHaveBeenCalledWith("acme")
      expect(api.getReleases).not.toHaveBeenCalledWith("management")
    })

    it("refreshes add-on status from the SSE stream", async () => {
      mockData([], [addonRelease("kyverno", { status: "Reconciling" })])
      renderWithAuthz(<TopologyView />)
      expect(
        await screen.findByTestId("management-addon-status-kyverno")
      ).toHaveTextContent("Reconciling")
      vi.mocked(api.getReleases).mockResolvedValue([addonRelease("kyverno")])
      act(() => {
        stream!.onEvent({
          kind: "HelmRelease",
          namespace: "flux-system",
          name: "kyverno",
          status: "Ready",
          message: "Helm upgrade succeeded",
          timestamp: new Date().toISOString(),
        })
      })
      await waitFor(
        () =>
          expect(
            screen.getByTestId("management-addon-status-kyverno")
          ).toHaveTextContent("Ready"),
        { timeout: 4000 }
      )
    })

    it("never shows add-on rows to non-admins", async () => {
      mockData([], [release, addons[0]])
      renderWithAuthz(<TopologyView />, TENANT_MEMBER)
      await screen.findByRole("button", { name: "Inspect release podinfo" })
      expect(screen.queryByTestId("management-addon-row-kyverno")).toBeNull()
      expect(screen.queryByText("kyverno")).toBeNull()
      expect(screen.queryByText("Protected")).toBeNull()
      expect(api.getCoreAddons).not.toHaveBeenCalled()
    })

    it("shows demo add-on rows including optional forgejo and keel-server", () => {
      window.history.replaceState(null, "", "/?demo=true")
      renderWithAuthz(<TopologyView />)
      for (const id of ["flux2", "rauthy", "forgejo", "keel-server"]) {
        expect(addonRow(id)).toBeInTheDocument()
      }
      expect(within(addonRow("rauthy")).getByText("0.37.2")).toBeInTheDocument()
      expect(
        screen.getByRole("button", { name: "Upgrade rauthy" })
      ).toBeEnabled()
      expect(api.getCoreAddons).not.toHaveBeenCalled()
    })
  })
})
