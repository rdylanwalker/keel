import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { StackInspectorModal } from "./StackInspectorModal"
import * as api from "../lib/api"

vi.mock("../lib/api", () => ({
  getStackDiagnostics: vi.fn(),
  getReleaseDiagnostics: vi.fn(),
  restartStackWorkspace: vi.fn(),
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

    render(
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

  it("triggers rollout restart and handles release kind", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    vi.mocked(api.restartStackWorkspace).mockResolvedValue({
      restarted: true,
      namespace: "tenant-acme",
      name: "prod-cluster",
      message: "Workspace pod deleted for rollout restart",
    })

    render(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        onClose={onClose}
        onRestarted={onRestarted}
      />
    )

    await screen.findByText("Pod: Running")
    fireEvent.click(screen.getByRole("button", { name: /Rollout Restart/i }))

    expect(
      await screen.findByText("Workspace pod deleted for rollout restart")
    ).toBeInTheDocument()
    expect(onRestarted).toHaveBeenCalled()
  })

  it("loads release diagnostics and surfaces fetch errors", async () => {
    vi.mocked(api.getReleaseDiagnostics).mockRejectedValueOnce(
      new Error("Release not found")
    )

    render(
      <StackInspectorModal
        namespace="tenant-acme"
        name="podinfo"
        kind="release"
        onClose={onClose}
      />
    )

    expect(await screen.findByText("Release not found")).toBeInTheDocument()
    expect(
      screen.queryByRole("button", { name: /Rollout Restart/i })
    ).not.toBeInTheDocument()
  })

  it("renders SSH / Connect button for stacks and triggers onOpenSsh", async () => {
    vi.mocked(api.getStackDiagnostics).mockResolvedValue(sampleDiagnostics)
    const onOpenSsh = vi.fn()

    render(
      <StackInspectorModal
        namespace="tenant-acme"
        name="prod-cluster"
        kind="stack"
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

    render(
      <StackInspectorModal
        namespace="flux-system"
        name="management-cluster-core"
        kind="stack"
        provider="kind"
        onClose={onClose}
      />
    )

    const sshBtn = await screen.findByTestId("inspector-ssh-btn")
    expect(sshBtn).toBeDisabled()
  })
})
