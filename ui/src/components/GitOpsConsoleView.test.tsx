import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { GitOpsConsoleView } from "./GitOpsConsoleView"
import * as api from "../lib/api"
import { ReleaseSummary } from "../types/api"

vi.mock("../lib/api", () => ({
  getReleases: vi.fn(),
  createEventSourceStream: vi.fn(() => ({
    close: vi.fn(),
  })),
}))

const mockReleases: ReleaseSummary[] = [
  {
    id: "rel-test-01",
    name: "test-canary-app",
    tenant: "tenant-acme",
    namespace: "tenant-acme",
    type: "canary",
    status: "progressing",
    version: "v2.0.0",
    canaryWeight: 60,
    errorRate: 0.0,
    latencyMs: 12,
  },
]

describe("GitOpsConsoleView component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it("renders GitOps console and loads releases and event timeline", async () => {
    vi.mocked(api.getReleases).mockResolvedValueOnce(mockReleases)

    render(<GitOpsConsoleView />)

    expect(
      screen.getByText("GitOps & Progressive Delivery Console")
    ).toBeInTheDocument()
    expect(
      screen.getByText("Flagger Canary Traffic Rollouts (Envoy Gateway)")
    ).toBeInTheDocument()

    await waitFor(() => {
      expect(screen.getAllByText("test-canary-app")[0]).toBeInTheDocument()
      expect(screen.getByText("60%")).toBeInTheDocument()
    })
  })

  it("toggles pause/resume live SSE event stream", () => {
    render(<GitOpsConsoleView />)

    const pauseBtn = screen.getByRole("button", { name: /Pause Stream/i })
    fireEvent.click(pauseBtn)

    expect(
      screen.getByRole("button", { name: /Resume Stream/i })
    ).toBeInTheDocument()

    const resumeBtn = screen.getByRole("button", { name: /Resume Stream/i })
    fireEvent.click(resumeBtn)

    expect(
      screen.getByRole("button", { name: /Pause Stream/i })
    ).toBeInTheDocument()
  })

  it("clears event logs and filters events by CRD kind", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    render(<GitOpsConsoleView />)

    await waitFor(() => {
      expect(screen.getByText("flux-system/flux-system")).toBeInTheDocument()
    })

    // Filter by Kustomization
    const filterSelect = screen.getByLabelText(/Filter events by CRD kind/i)
    fireEvent.change(filterSelect, { target: { value: "Kustomization" } })

    expect(screen.getByText("tenant-acme/tenant-acme-apps")).toBeInTheDocument()
    expect(
      screen.queryByText("flux-system/flux-system")
    ).not.toBeInTheDocument()

    // Clear logs
    const clearBtn = screen.getByTitle("Clear event logs")
    fireEvent.click(clearBtn)

    expect(
      screen.getByText("No CRD events received yet or stream cleared.")
    ).toBeInTheDocument()
    window.history.replaceState({}, "", "/")
  })

  it("renders clean empty canary state when getReleases returns empty array in non-demo mode", async () => {
    vi.mocked(api.getReleases).mockResolvedValueOnce([])

    render(<GitOpsConsoleView />)

    await waitFor(() => {
      expect(
        screen.getByText("No active canary traffic rollouts found.")
      ).toBeInTheDocument()
    })
  })

  it("initializes with DEFAULT_EVENTS and DEFAULT_RELEASES when demo=true is in location search", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getReleases).mockResolvedValueOnce([])

    render(<GitOpsConsoleView />)

    await waitFor(() => {
      expect(screen.getAllByText("frontend-portal")[0]).toBeInTheDocument()
      expect(screen.getByText("flux-system/flux-system")).toBeInTheDocument()
    })
    window.history.replaceState({}, "", "/")
  })

  it("handles event source stream error callback", () => {
    let errorCb: (err: Event) => void = () => {}
    vi.mocked(api.createEventSourceStream).mockImplementationOnce(
      (_, onError) => {
        if (onError) errorCb = onError
        return { close: vi.fn() } as unknown as EventSource
      }
    )

    render(<GitOpsConsoleView />)
    errorCb(new Event("error"))
  })

  it("handles exception during stream creation cleanly", () => {
    vi.mocked(api.createEventSourceStream).mockImplementationOnce(() => {
      throw new Error("Stream failed")
    })

    render(<GitOpsConsoleView />)
  })
})
