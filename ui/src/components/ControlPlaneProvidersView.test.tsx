import { screen, fireEvent, waitFor, within } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import { ControlPlaneProvidersView } from "./ControlPlaneProvidersView"
import * as api from "../lib/api"
import { ApiError } from "../lib/errors"
import { buildAuthz } from "../lib/authz"
import { renderWithAuthz } from "../test/render"
import { CatalogItem } from "../types/api"

vi.mock("../lib/api", () => ({
  getStacksWithMeta: vi.fn(),
  getProviders: vi.fn(),
  getProposals: vi.fn().mockResolvedValue([]),
  getCoreAddons: vi.fn().mockResolvedValue([]),
  getManagementAddons: vi.fn(),
  toggleProvider: vi.fn(),
  rollbackManagementAddon: vi.fn(),
  upgradeManagementAddon: vi.fn(),
}))

const TENANT_USER = buildAuthz({
  authenticated: true,
  phase: "oidc-active",
  groups: ["acme"],
  isAdmin: false,
})

function addon(id: string, extra: Partial<CatalogItem> = {}): CatalogItem {
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
    vi.mocked(api.getManagementAddons).mockResolvedValue({
      addons: [],
      liveStatusAvailable: true,
    })
  })

  afterEach(() => {
    window.history.replaceState({}, "", "/")
  })

  it("renders only real provider registry fields (no fixture names/regions)", async () => {
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
      {
        id: "gcp",
        enabled: true,
        description: "Google Cloud Platform",
        package: "pulumi/gcp",
      },
    ])

    renderWithAuthz(<ControlPlaneProvidersView />)

    expect(screen.getByText("Control Plane & Providers")).toBeInTheDocument()
    expect(
      screen.getByText("Management Control Plane Cluster")
    ).toBeInTheDocument()

    await waitFor(() => {
      expect(screen.getByText("Google Cloud Platform")).toBeInTheDocument()
      expect(
        screen.getByText(
          "GitOps: Flux CD v2 · Forge: Forgejo · Secrets: SOPS + age / S3"
        )
      ).toBeInTheDocument()
    })
    expect(screen.getByText("pulumi/gcp")).toBeInTheDocument()
    expect(within(screen.getByTestId("provider-aws")).getByText("aws"))
    // Fixture-only details never appear outside demo mode
    expect(screen.queryByText("Amazon Web Services")).not.toBeInTheDocument()
    expect(screen.queryByText(/us-east-1/)).not.toBeInTheDocument()
    expect(screen.queryByText("Proxmox VE Cluster")).not.toBeInTheDocument()
  })

  it("shows an empty state when no providers are registered", async () => {
    renderWithAuthz(<ControlPlaneProvidersView />)
    await waitFor(() => {
      expect(
        screen.getByText("No infrastructure providers registered.")
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

    renderWithAuthz(
      <ControlPlaneProvidersView onNavigateToCatalog={onNavigateToCatalog} />
    )

    await waitFor(() => {
      expect(screen.getByText("Custom Cloud Provider")).toBeInTheDocument()
    })

    fireEvent.click(screen.getAllByRole("button", { name: /Provision →/i })[0])
    expect(onNavigateToCatalog).toHaveBeenCalled()

    fireEvent.click(screen.getAllByRole("button", { name: /^Enable$/i })[0])

    await waitFor(() => {
      expect(api.toggleProvider).toHaveBeenCalledWith("gcp", true)
      expect(
        screen.getByText("PR #789 opened on branch feature/enable-gcp")
      ).toBeInTheDocument()
      expect(
        screen.getByRole("button", { name: "Go to PR #789 →" })
      ).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /Dismiss/i }))
    expect(
      screen.queryByText("PR #789 opened on branch feature/enable-gcp")
    ).not.toBeInTheDocument()
  })

  it("surfaces friendly API errors when a provider toggle fails", async () => {
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
    ])
    vi.mocked(api.toggleProvider)
      .mockRejectedValueOnce("Toggle error string")
      .mockRejectedValueOnce(
        new ApiError(500, "Internal server error", { correlationId: "c-1" })
      )

    renderWithAuthz(<ControlPlaneProvidersView />)
    await waitFor(() => {
      expect(screen.getByRole("button", { name: /^Disable$/i }))
    })

    fireEvent.click(screen.getByRole("button", { name: /^Disable$/i }))
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to toggle provider aws"
      )
    })
    fireEvent.click(screen.getByRole("button", { name: /^Disable$/i }))
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Internal server error (reference: c-1)"
      )
    })
  })

  it("refreshes data on Refresh click and shows load errors", async () => {
    renderWithAuthz(<ControlPlaneProvidersView />)
    await waitFor(() => expect(api.getProviders).toHaveBeenCalledTimes(1))

    vi.mocked(api.getProviders).mockRejectedValueOnce(
      new ApiError(403, "Access denied")
    )
    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))
    await waitFor(() => {
      expect(api.getProviders).toHaveBeenCalledTimes(2)
      expect(screen.getByRole("alert")).toHaveTextContent("Access denied")
    })

    vi.mocked(api.getProviders).mockRejectedValueOnce("non-error failure")
    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to load control plane & provider state"
      )
    })
  })

  it("shows Go to PR for pending provider toggle PRs", async () => {
    const onNavigateToPreview = vi.fn()
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true },
    ])
    vi.mocked(api.getProposals).mockResolvedValueOnce([
      {
        id: "999",
        title: "Toggle AWS",
        branch: "keel/providers/toggle-aws-1234",
        author: "admin",
        tenant: "keel-system",
        status: "open",
        createdAt: "now",
      },
      {
        id: "1000",
        title: "Old",
        branch: "keel/providers/toggle-aws-1",
        author: "admin",
        tenant: "keel-system",
        status: "merged",
        createdAt: "now",
      },
    ])

    renderWithAuthz(
      <ControlPlaneProvidersView onNavigateToPreview={onNavigateToPreview} />
    )

    const goBtn = await screen.findByRole("button", {
      name: "Go to PR #999 →",
    })
    fireEvent.click(goBtn)
    expect(onNavigateToPreview).toHaveBeenCalled()
  })

  it("does not load data when unauthenticated", () => {
    renderWithAuthz(<ControlPlaneProvidersView authenticated={false} />)
    expect(api.getProviders).not.toHaveBeenCalled()
  })

  it("hides admin-only controls and management sections for non-admins", async () => {
    vi.mocked(api.getProviders).mockResolvedValueOnce([
      { id: "aws", enabled: true, description: "AWS" },
      { id: "gcp", enabled: false },
    ])

    renderWithAuthz(<ControlPlaneProvidersView />, TENANT_USER)

    await waitFor(() => {
      expect(screen.getByText("AWS")).toBeInTheDocument()
    })
    expect(
      screen.queryByRole("button", { name: /^(Enable|Disable)$/ })
    ).not.toBeInTheDocument()
    expect(
      screen.queryByText("Management Control Plane Cluster")
    ).not.toBeInTheDocument()
    expect(
      screen.queryByText("Management Plane Base Addons")
    ).not.toBeInTheDocument()
    expect(api.getCoreAddons).not.toHaveBeenCalled()
    expect(api.getManagementAddons).not.toHaveBeenCalled()
    expect(api.getProposals).not.toHaveBeenCalled()
  })

  it("maps add-on status by namespace/name and never by chart name", async () => {
    vi.mocked(api.getCoreAddons).mockResolvedValueOnce([
      addon("kyverno"),
      addon("flagger", { version: "1.40.0" }),
      addon("prometheus"),
      addon("cert-manager", { version: "v1.21.3" }),
      addon("rauthy", {
        serviceUrl: "https://idp.keel.local:8888/auth/v1/admin",
      }),
    ])
    vi.mocked(api.getManagementAddons).mockResolvedValueOnce({
      liveStatusAvailable: true,
      addons: [
        {
          id: "kyverno",
          name: "kyverno",
          namespace: "keel-system",
          chart: "kyverno",
          desiredVersion: "3.3.4",
          status: "Ready",
          tracked: true,
        },
        {
          id: "prometheus",
          name: "prometheus",
          namespace: "keel-system",
          status: "Reconciling",
          tracked: true,
        },
        {
          id: "flagger",
          name: "flagger",
          namespace: "keel-system",
          tracked: false,
        },
        {
          id: "extra-addon",
          name: "extra-addon",
          namespace: "flux-system",
          status: "Error",
          tracked: true,
        },
      ],
    })

    renderWithAuthz(<ControlPlaneProvidersView />)

    await waitFor(() => {
      expect(screen.getByTestId("core-addon-kyverno")).toBeInTheDocument()
    })
    expect(screen.getByTestId("core-addon-status-kyverno")).toHaveTextContent(
      "Ready"
    )
    expect(
      within(screen.getByTestId("core-addon-kyverno")).getByText("v3.3.4")
    ).toBeInTheDocument()
    expect(
      screen.getByTestId("core-addon-status-prometheus")
    ).toHaveTextContent("Reconciling")
    expect(screen.getByTestId("core-addon-status-flagger")).toHaveTextContent(
      "Not tracked (installed by bootstrap)"
    )
    // absent from endpoint entirely
    expect(screen.getByTestId("core-addon-status-rauthy")).toHaveTextContent(
      "Not tracked (installed by bootstrap)"
    )
    expect(
      screen.getByTestId("core-addon-status-extra-addon")
    ).toHaveTextContent("Failed")
    expect(screen.getByText("v1.21.3")).toBeInTheDocument()
    expect(screen.queryByText("vv1.21.3")).not.toBeInTheDocument()
    expect(
      screen.getByRole("link", {
        name: "https://idp.keel.local:8888/auth/v1/admin",
      })
    ).toHaveAttribute("href", "https://idp.keel.local:8888/auth/v1/admin")
    expect(screen.getAllByText("Protected")).toHaveLength(6)
    // Health is derived from real statuses, never assumed
    expect(screen.getByTestId("management-health")).toHaveTextContent(
      "Degraded"
    )
    expect(screen.queryByText("Healthy")).not.toBeInTheDocument()
  })

  it("derives Healthy only when all tracked add-ons are ready", async () => {
    vi.mocked(api.getCoreAddons).mockResolvedValueOnce([addon("kyverno")])
    vi.mocked(api.getManagementAddons).mockResolvedValueOnce({
      liveStatusAvailable: true,
      addons: [
        {
          id: "kyverno",
          name: "kyverno",
          namespace: "keel-system",
          status: "Ready",
          tracked: true,
        },
      ],
    })
    renderWithAuthz(<ControlPlaneProvidersView />)
    await waitFor(() => {
      expect(screen.getByTestId("management-health")).toHaveTextContent(
        "Healthy"
      )
    })
  })

  it("reports unknown health when status endpoint is unavailable or offline", async () => {
    vi.mocked(api.getCoreAddons).mockResolvedValue([addon("kyverno")])
    vi.mocked(api.getManagementAddons).mockRejectedValueOnce(
      new ApiError(404, "not found")
    )
    renderWithAuthz(<ControlPlaneProvidersView />)
    await waitFor(() => {
      expect(screen.getByTestId("core-addon-status-kyverno")).toHaveTextContent(
        "Not tracked (installed by bootstrap)"
      )
    })
    expect(screen.getByTestId("management-health")).toHaveTextContent(
      "Status unknown"
    )

    vi.mocked(api.getManagementAddons).mockResolvedValueOnce({
      liveStatusAvailable: false,
      addons: [],
    })
    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))
    await waitFor(() => {
      expect(screen.getByTestId("management-health")).toHaveTextContent(
        "No live cluster connection"
      )
    })
  })

  it("opens the upgrade modal for a management add-on", async () => {
    vi.mocked(api.getCoreAddons).mockResolvedValueOnce([
      addon("kyverno", { name: "Kyverno" }),
    ])
    renderWithAuthz(<ControlPlaneProvidersView />)
    fireEvent.click(
      await screen.findByRole("button", { name: /Upgrade \/ Configure/i })
    )
    expect(
      screen.getByRole("dialog", { name: /Upgrade Kyverno/i })
    ).toBeInTheDocument()
  })

  it("confirms before opening a rollback PR and reports failures", async () => {
    vi.mocked(api.getCoreAddons).mockResolvedValueOnce([
      addon("kyverno"),
      addon("flagger"),
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
        new ApiError(400, "No previous revision found to roll back to")
      )

    renderWithAuthz(<ControlPlaneProvidersView />)
    await waitFor(() => {
      expect(screen.getByTestId("core-addon-flagger")).toBeInTheDocument()
    })

    const rollbackBtns = screen.getAllByRole("button", {
      name: /Rollback PR/i,
    })

    // Cancel does nothing
    fireEvent.click(rollbackBtns[0])
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }))
    expect(api.rollbackManagementAddon).not.toHaveBeenCalled()

    fireEvent.click(rollbackBtns[0])
    expect(
      await screen.findByRole("alertdialog", { name: /Roll back kyverno/i })
    ).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: "Open Rollback PR" }))
    await waitFor(() => {
      expect(api.rollbackManagementAddon).toHaveBeenCalledWith("kyverno")
      expect(
        screen.getByText("PR #501 opened on branch keel/mgmt/rollback-kyverno")
      ).toBeInTheDocument()
    })

    fireEvent.click(rollbackBtns[1])
    fireEvent.click(
      await screen.findByRole("button", { name: "Open Rollback PR" })
    )
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "No previous revision found to roll back to"
      )
    })
  })

  it("renders demo fixtures without any live API calls in demo mode", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    renderWithAuthz(<ControlPlaneProvidersView />)

    expect(screen.getByText("Proxmox VE Cluster")).toBeInTheDocument()
    expect(screen.getByTestId("core-addon-kyverno")).toBeInTheDocument()
    expect(screen.getByTestId("management-health")).toHaveTextContent("Healthy")
    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))
    expect(api.getProviders).not.toHaveBeenCalled()
    expect(api.getStacksWithMeta).not.toHaveBeenCalled()
  })
})
