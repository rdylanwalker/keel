import { screen, fireEvent, waitFor, act } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { StackInspectorModal } from "./StackInspectorModal"
import * as api from "../lib/api"
import { ApiError } from "../lib/errors"
import { ANONYMOUS_AUTHZ, buildAuthz } from "../lib/authz"
import { renderWithAuthz } from "../test/render"

vi.mock("../lib/api", () => ({
  getStackDiagnostics: vi.fn(),
  getReleaseDiagnostics: vi.fn(),
  restartStackWorkspace: vi.fn(),
  forceDeleteStack: vi.fn(),
  unlockStack: vi.fn(),
}))

const sampleDiagnostics = {
  namespace: "tenant-acme",
  name: "prod-cluster",
  workspacePhase: "Running",
  metadata: {
    kind: "Stack",
    apiVersion: "pulumi.com/v1",
    creationTimestamp: "2026-11-15T12:00:00Z",
    uid: "uid-1234",
    specSummary: { stack: "acme/prod", backend: "s3://pulumi-state" },
    labels: { tenant: "acme" },
    annotations: { "keel.io/managed": "true" },
  },
  conditions: [
    {
      type: "Ready",
      status: "True",
      reason: "Succeeded",
      message: "Stack reconciled",
    },
    {
      type: "Reconciling",
      status: "True",
      reason: "Progressing",
      message: "Updating resources",
    },
  ],
  events: [
    {
      eventType: "Normal",
      reason: "Reconciled",
      message: "Workspace pod ready",
      count: 1,
      lastTimestamp: "2026-11-15T12:01:00Z",
    },
    {
      eventType: "Warning",
      reason: "BackOff",
      message: "Retrying transient error",
      count: 2,
      lastTimestamp: "2026-11-15T12:02:00Z",
    },
  ],
  logs: ["2026-11-15T12:00:01Z INFO Starting workspace"],
}

describe("StackInspectorModal component", () => {
  const onClose = vi.fn()
  const onRestarted = vi.fn()

  beforeEach(() => {
    vi.clearAllMocks()
  })

  it("loads and renders stack diagnostics, metadata, conditions, events, and logs", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={onClose}
        onRestarted={onRestarted}
      />
    )

    expect(
      await screen.findByText("tenant-acme/prod-cluster")
    ).toBeInTheDocument()
    expect(await screen.findByText("Pod: Running")).toBeInTheDocument()
    expect(screen.getByText("Resource Metadata")).toBeInTheDocument()
    expect(screen.getByText("Stack reconciled")).toBeInTheDocument()
    expect(screen.getByText("Workspace pod ready")).toBeInTheDocument()
    expect(screen.getByText("Retrying transient error")).toBeInTheDocument()

    // Toggle auto-refresh, fullscreen, refresh, and close
    fireEvent.click(screen.getByRole("button", { name: /Auto: 5s/i }))
    expect(
      screen.getByRole("button", { name: /Auto: Off/i })
    ).toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: /Expand fullscreen/i }))
    expect(
      screen.getByRole("button", { name: /Exit fullscreen/i })
    ).toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: /Refresh/i }))
    await waitFor(() => {
      expect(api.getStackDiagnostics).toHaveBeenCalledTimes(2)
    })

    fireEvent.click(screen.getByRole("button", { name: /Close inspector/i }))
    expect(onClose).toHaveBeenCalled()
  })

  it("triggers rollout restart from Advanced section and handles release kind", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    vi.mocked(api.restartStackWorkspace).mockResolvedValue({
      restarted: true,
      namespace: "tenant-acme",
      name: "prod-cluster",
      message: "Workspace pod deleted for rollout restart",
    })

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={onClose}
        onRestarted={onRestarted}
      />
    )

    await screen.findByText("Pod: Running")
    expect(
      screen.queryByRole("button", { name: /Restart Workspace/i })
    ).not.toBeInTheDocument()
    fireEvent.click(screen.getByTestId("inspector-advanced-toggle"))
    fireEvent.click(screen.getByRole("button", { name: /Restart Workspace/i }))

    expect(
      await screen.findByText("Workspace pod deleted for rollout restart")
    ).toBeInTheDocument()
    expect(onRestarted).toHaveBeenCalled()
  })

  it("loads release diagnostics and surfaces fetch errors", async () => {
    vi.mocked(api.getReleaseDiagnostics).mockRejectedValueOnce(
      new Error("Release not found")
    )

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="podinfo"
        kind="release"
        onClose={onClose}
      />
    )

    expect(await screen.findByText("Release not found")).toBeInTheDocument()
    expect(
      screen.queryByTestId("inspector-advanced-toggle")
    ).not.toBeInTheDocument()
    // UI-11: release-appropriate section labels
    expect(screen.getByText("Release Conditions")).toBeInTheDocument()
    expect(screen.getByText(/Release Events \(0\)/)).toBeInTheDocument()
    expect(screen.getByText(/Application Pod Logs/)).toBeInTheDocument()
    expect(screen.queryByText("Stack Conditions")).not.toBeInTheDocument()
    expect(screen.queryByText(/Workspace Pod Logs/)).not.toBeInTheDocument()
  })

  it("uses stack labels for PKO stacks", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={onClose}
      />
    )
    expect(await screen.findByText("Stack Conditions")).toBeInTheDocument()
    expect(screen.getByText(/Workspace Pod Logs/)).toBeInTheDocument()
    expect(screen.getByText(/Recent Kubernetes Events/)).toBeInTheDocument()
  })

  it("shows correlation id for server errors", async () => {
    vi.mocked(api.getStackDiagnostics).mockRejectedValueOnce(
      new ApiError(500, "Internal server error", { correlationId: "corr-42" })
    )
    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={onClose}
      />
    )
    expect(
      await screen.findByText("Internal server error (reference: corr-42)")
    ).toBeInTheDocument()
  })

  it("disables unlock/force-delete/restart for non tenant-admins (read-only)", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    const member = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["tenant-acme"],
    })
    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        tenant="acme"
        onClose={onClose}
      />,
      member
    )
    await screen.findByText("Pod: Running")
    expect(screen.getByTestId("inspector-unlock-btn")).toBeDisabled()
    expect(screen.getByTestId("inspector-force-delete-btn")).toBeDisabled()
    expect(screen.getByTestId("inspector-force-delete-btn")).toHaveAttribute(
      "title",
      "Tenant administrator privileges required"
    )
    expect(screen.getByTestId("inspector-readonly-note")).toBeInTheDocument()
    fireEvent.click(screen.getByTestId("inspector-advanced-toggle"))
    expect(screen.getByTestId("inspector-restart-workspace-btn")).toBeDisabled()
  })

  it("enables operational actions for the tenant admin of the stack's tenant", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    const tenantAdmin = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["tenant-acme-admin"],
    })
    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={onClose}
      />,
      tenantAdmin
    )
    await screen.findByText("Pod: Running")
    expect(screen.getByTestId("inspector-unlock-btn")).toBeEnabled()
    expect(screen.getByTestId("inspector-force-delete-btn")).toBeEnabled()
    expect(
      screen.queryByTestId("inspector-readonly-note")
    ).not.toBeInTheDocument()
  })

  it("restricts management-plane stack actions to cluster admins", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    const tenantAdmin = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["flux-system-admin"],
    })
    renderWithAuthz(
      <StackInspectorModal
        namespace="flux-system"
        name="management-cluster-core"
        onClose={onClose}
      />,
      tenantAdmin
    )
    await screen.findByText("Pod: Running")
    expect(screen.getByTestId("inspector-unlock-btn")).toBeDisabled()
    expect(screen.getByTestId("inspector-unlock-btn")).toHaveAttribute(
      "title",
      "Cluster administrator privileges required for management-plane stacks"
    )
  })

  it("treats anonymous callers as read-only and closes on Escape and route change", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    const close = vi.fn()
    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={close}
      />,
      ANONYMOUS_AUTHZ
    )
    await screen.findByText("Pod: Running")
    expect(screen.getByTestId("inspector-unlock-btn")).toBeDisabled()
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" })
    expect(close).toHaveBeenCalledTimes(1)
    act(() => {
      window.dispatchEvent(new HashChangeEvent("hashchange"))
    })
    expect(close).toHaveBeenCalledTimes(2)
  })

  it("renders SSH / Connect button for stacks and triggers onOpenSsh", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    const onOpenSsh = vi.fn()

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-vm"
        kind="stack"
        provider="proxmox"
        resourceKind="vm"
        onClose={onClose}
        onOpenSsh={onOpenSsh}
      />
    )

    const sshBtn = await screen.findByTestId("inspector-ssh-btn")
    expect(sshBtn).toBeEnabled()
    fireEvent.click(sshBtn)
    expect(onOpenSsh).toHaveBeenCalled()
  })

  it("disables SSH / Connect button for kind and non-VM stacks", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)

    renderWithAuthz(
      <StackInspectorModal
        namespace="flux-system"
        name="management-cluster-core"
        kind="stack"
        provider="kind"
        resourceKind="cluster"
        onClose={onClose}
      />
    )

    const sshBtn = await screen.findByTestId("inspector-ssh-btn")
    expect(sshBtn).toBeDisabled()
    expect(
      screen.queryByTestId("inspector-force-delete-btn")
    ).not.toBeInTheDocument()
  })

  it("confirms and executes Force Delete for non-protected stacks", async () => {
    const onForceDeleted = vi.fn()
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    vi.mocked(api.forceDeleteStack).mockResolvedValue({
      forceDeleted: true,
      namespace: "tenant-default",
      name: "azure-free-tier-vm",
      message:
        "Cleared finalizers on terminating Stack tenant-default/azure-free-tier-vm",
    })

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-default"
        name="azure-free-tier-vm"
        kind="stack"
        onClose={onClose}
        onForceDeleted={onForceDeleted}
      />
    )

    const forceDeleteBtn = await screen.findByTestId(
      "inspector-force-delete-btn"
    )
    fireEvent.click(forceDeleteBtn)

    const confirmBtn = await screen.findByTestId("confirm-force-delete-btn")
    fireEvent.click(confirmBtn)

    await waitFor(() => {
      expect(api.forceDeleteStack).toHaveBeenCalledWith(
        "tenant-default",
        "azure-free-tier-vm"
      )
      expect(onForceDeleted).toHaveBeenCalled()
      expect(
        screen.getByText(/Cleared finalizers on terminating Stack/i)
      ).toBeInTheDocument()
    })
  })

  it("allows cancelling Force Delete and surfaces errors on failure", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    vi.mocked(api.forceDeleteStack)
      .mockRejectedValueOnce(new Error("Forbidden: protected stack"))
      .mockRejectedValueOnce("unexpected string error")

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-default"
        name="azure-free-tier-vm"
        kind="stack"
        onClose={onClose}
        onRestarted={onRestarted}
      />
    )

    fireEvent.click(await screen.findByTestId("inspector-force-delete-btn"))
    expect(
      await screen.findByRole("alertdialog", {
        name: "Force delete this Stack?",
      })
    ).toBeInTheDocument()
    expect(screen.getByTestId("confirm-force-delete-btn")).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }))
    await waitFor(() =>
      expect(
        screen.queryByTestId("confirm-force-delete-btn")
      ).not.toBeInTheDocument()
    )
    expect(api.forceDeleteStack).not.toHaveBeenCalled()

    // First failure: Error instance
    fireEvent.click(screen.getByTestId("inspector-force-delete-btn"))
    fireEvent.click(await screen.findByTestId("confirm-force-delete-btn"))
    expect(
      await screen.findByText("Forbidden: protected stack")
    ).toBeInTheDocument()

    // Confirming closes the dialog; the second attempt re-opens it.
    await waitFor(() =>
      expect(
        screen.queryByTestId("confirm-force-delete-btn")
      ).not.toBeInTheDocument()
    )

    // Second failure: non-Error thrown
    fireEvent.click(screen.getByTestId("inspector-force-delete-btn"))
    fireEvent.click(await screen.findByTestId("confirm-force-delete-btn"))
    expect(
      await screen.findByText("Failed to force-delete stack")
    ).toBeInTheDocument()
  })

  it("shows locked-stack banner and executes Unlock Stack", async () => {
    const onUnlocked = vi.fn()
    vi.mocked(api.getStackDiagnostics).mockResolvedValue({
      ...sampleDiagnostics,
      logs: [
        "error: the stack is currently locked by 1 lock(s). Either wait for the other process(es) to end or delete the lock file with `pulumi cancel`.",
      ],
    })
    vi.mocked(api.unlockStack).mockResolvedValue({
      unlocked: true,
      namespace: "tenant-default",
      name: "azure-free-tier-vm",
      message: "Unlocked stack tenant-default/azure-free-tier-vm",
    })

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-default"
        name="azure-free-tier-vm"
        kind="stack"
        onClose={onClose}
        onUnlocked={onUnlocked}
      />
    )

    expect(
      await screen.findByText(/A stale Pulumi state lock was detected/i)
    ).toBeInTheDocument()

    fireEvent.click(screen.getByTestId("inspector-unlock-btn"))

    await waitFor(() => {
      expect(api.unlockStack).toHaveBeenCalledWith(
        "tenant-default",
        "azure-free-tier-vm"
      )
      expect(onUnlocked).toHaveBeenCalled()
      expect(
        screen.getByText("Unlocked stack tenant-default/azure-free-tier-vm")
      ).toBeInTheDocument()
    })
  })

  it("shows terminating banner with workspace restart guidance when terminating stack awaits workspace readiness", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue({
      ...sampleDiagnostics,
      metadata: {
        ...sampleDiagnostics.metadata,
        deletionTimestamp: "2026-11-19T05:26:42Z",
      },
      conditions: [
        {
          type: "Reconciling",
          status: "True",
          reason: "StackProcessing",
          message: "waiting for workspace readiness",
        },
      ],
    })

    renderWithAuthz(
      <StackInspectorModal
        namespace="tenant-default"
        name="azure-free-tier-vm"
        kind="stack"
        onClose={onClose}
      />
    )

    const banner = await screen.findByTestId("inspector-terminating-banner")
    expect(banner).toHaveTextContent(/waiting for its workspace pod/i)

    fireEvent.click(screen.getByTestId("inspector-force-delete-btn"))
    expect(
      await screen.findByText(/try Restart Workspace first/i)
    ).toBeInTheDocument()
  })
})
