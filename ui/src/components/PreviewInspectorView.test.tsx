import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { PreviewInspectorView } from "./PreviewInspectorView"
import * as api from "../lib/api"
import { PreviewReport, Proposal } from "../types/api"

vi.mock("../lib/api", () => ({
  getProposals: vi.fn(),
  getProposalPreview: vi.fn(),
  mergeProposal: vi.fn(),
  rejectProposal: vi.fn(),
  reopenProposal: vi.fn(),
}))

const mockProposals: Proposal[] = [
  {
    id: "101",
    title: "Deploy Redis Cache",
    branch: "keel/tenant-acme/redis-101",
    author: "dev@acme.com",
    tenant: "tenant-acme",
    status: "open",
    createdAt: "5 mins ago",
    prUrl: "https://git.internal/pr/101",
    previewStatus: "ready",
  },
]

const mockPreview: PreviewReport = {
  prUrl: "https://git.internal/pr/101",
  policyStatus: "passed",
  summary: {
    create: 1,
    update: 1,
    delete: 1,
    replace: 1,
    same: 5,
    drift: 0,
    total: 9,
  },
  resources: [
    {
      urn: "urn:pulumi:prod::redis::k8s:Deployment::redis-master",
      resource_type: "k8s:Deployment",
      name: "redis-master",
      op: "Create",
      changed_fields: ["spec.replicas"],
    },
    {
      urn: "urn:pulumi:prod::redis::k8s:ConfigMap::redis-config",
      resource_type: "k8s:ConfigMap",
      name: "redis-config",
      op: "Update",
      changed_fields: ["data.maxmemory"],
    },
    {
      urn: "urn:pulumi:prod::redis::k8s:Service::redis-svc",
      resource_type: "k8s:Service",
      name: "redis-svc",
      op: "Delete",
      changed_fields: [],
    },
    {
      urn: "urn:pulumi:prod::redis::k8s:Pod::redis-pod",
      resource_type: "k8s:Pod",
      name: "redis-pod",
      op: "Replace",
      changed_fields: ["spec.containers"],
    },
  ],
}

describe("PreviewInspectorView component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it("renders PR preview inspector and loads proposals and diff tree", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    render(<PreviewInspectorView />)

    expect(
      screen.getByText("PR & PKO v2 Preview Inspector")
    ).toBeInTheDocument()

    await waitFor(() => {
      expect(
        screen.getByText("PR #101: Deploy Redis Cache")
      ).toBeInTheDocument()
      expect(screen.getByText("redis-master")).toBeInTheDocument()
      expect(screen.getAllByText("Create").length).toBeGreaterThan(0)
      expect(screen.getAllByText("Update").length).toBeGreaterThan(0)
      expect(
        screen.getByRole("link", { name: /Forge Link/i })
      ).toBeInTheDocument()
      expect(
        screen.getAllByRole("tooltip").map((el) => el.textContent)
      ).toEqual(["Forge Link", "Reject PR", "Merge PR Proposal"])
    })
  })

  it("merges PR successfully when Merge PR button is clicked", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.mergeProposal).mockResolvedValue({
      success: true,
      message: "Merged cleanly into main",
    })

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(
        screen.getByText("PR #101: Deploy Redis Cache")
      ).toBeInTheDocument()
    })

    const mergeBtn = screen.getByRole("button", { name: /Merge PR Proposal/i })
    fireEvent.click(mergeBtn)

    await waitFor(() => {
      expect(api.mergeProposal).toHaveBeenCalledWith("101")
      expect(screen.getByText("Merged cleanly into main")).toBeInTheDocument()
      expect(screen.getByRole("status")).toHaveTextContent(
        "Merged cleanly into main"
      )
      const rejectBtn = screen.getByRole("button", { name: /Reject PR/i })
      expect(mergeBtn).toHaveAttribute("aria-label", "Merged")
      expect(mergeBtn).toBeDisabled()
      expect(rejectBtn).toBeDisabled()
      expect(mergeBtn.className).toContain("disabled:cursor-not-allowed")
      expect(rejectBtn.className).toContain("disabled:cursor-not-allowed")
      expect(mergeBtn.className).toContain("enabled:hover:bg-purple-500")
      expect(rejectBtn.className).toContain("enabled:hover:bg-rose-600/30")
    })

    // Verify snackbar dismiss button closes notification
    fireEvent.click(
      screen.getByRole("button", { name: /Dismiss notification/i })
    )
    expect(
      screen.queryByText("Merged cleanly into main")
    ).not.toBeInTheDocument()
  })

  it("handles merge failure and preview fetch error gracefully", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockRejectedValue(
      new Error("PKO preview runner unavailable")
    )

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(
        screen.getByText("PKO preview runner unavailable")
      ).toBeInTheDocument()
    })

    vi.mocked(api.mergeProposal).mockResolvedValue({
      success: false,
      message: "Merge conflicts detected",
    })

    const mergeBtn = screen.getByRole("button", { name: /Merge PR Proposal/i })
    fireEvent.click(mergeBtn)

    await waitFor(() => {
      expect(screen.getByText("Merge conflicts detected")).toBeInTheDocument()
    })

    vi.mocked(api.rejectProposal).mockResolvedValue({
      success: true,
      message: "PR #101 rejected and closed.",
    })
    fireEvent.click(screen.getByRole("button", { name: /Reject PR/i }))
    await waitFor(() => {
      expect(
        screen.getByText("PR #101 rejected and closed.")
      ).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^merged$/i }))
    await waitFor(() => {
      expect(api.getProposals).toHaveBeenCalledWith(undefined, "merged")
    })
  })

  it("displays error when rejectProposal fails", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([
      { ...mockProposals[0], status: "open" },
    ])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.rejectProposal).mockResolvedValueOnce({
      success: false,
      message: "Cannot reject merged PR",
    })

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getAllByText(/Deploy Redis Cache/i)[0]).toBeInTheDocument()
      expect(api.getProposalPreview).toHaveBeenCalled()
    })

    fireEvent.click(screen.getByRole("button", { name: /Reject PR/i }))
    await waitFor(() => {
      expect(screen.getByText("Cannot reject merged PR")).toBeInTheDocument()
    })
  })

  it("reopens a closed PR when Reopen PR button is clicked", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([
      { ...mockProposals[0], status: "closed" },
    ])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.reopenProposal).mockResolvedValueOnce({
      success: true,
      message: "PR #101 reopened.",
    })

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getAllByText(/Deploy Redis Cache/i)[0]).toBeInTheDocument()
    })

    const reopenBtn = screen.getByRole("button", { name: /Reopen PR/i })
    fireEvent.click(reopenBtn)

    await waitFor(() => {
      expect(api.reopenProposal).toHaveBeenCalledWith("101")
      expect(screen.getByText("PR #101 reopened.")).toBeInTheDocument()
      expect(
        screen.getByRole("button", { name: /Merge PR Proposal/i })
      ).toBeEnabled()
      expect(screen.getByRole("button", { name: /Reject PR/i })).toBeEnabled()
    })
  })

  it("renders clean empty state when getProposals returns empty array in non-demo mode", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([])

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(
        screen.getByText(
          "No open proposals found — create a proposal from the Catalog."
        )
      ).toBeInTheDocument()
      expect(
        screen.getByText(
          "No proposal selected. Create a proposal from the Catalog to inspect PKO preview diffs."
        )
      ).toBeInTheDocument()
    })
  })

  it("initializes with DEFAULT_PROPOSALS and DEFAULT_PREVIEW when demo=true is in location search", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getProposals).mockResolvedValue([])
    vi.mocked(api.getProposalPreview).mockRejectedValueOnce(
      new Error("Preview unavailable in demo")
    )

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByText(/PR #\d+:/)).toBeInTheDocument()
    })
    window.history.replaceState({}, "", "/")
  })

  it("handles keyboard selection on PR items and string error on merge failure", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.mergeProposal).mockRejectedValue("Server timeout")

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(
        screen.getByText("PR #101: Deploy Redis Cache")
      ).toBeInTheDocument()
    })

    const prCard = screen.getByText("PR #101").closest('[role="button"]')!
    fireEvent.keyDown(prCard, { key: "Enter" })
    fireEvent.keyDown(prCard, { key: " " })

    const mergeBtn = screen.getByRole("button", { name: /Merge PR Proposal/i })
    fireEvent.click(mergeBtn)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Merge operation failed"
      )
    })
  })

  it("handles preview fetch string rejection", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockRejectedValue("Preview offline")

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to load PKO v2 preview diff"
      )
    })
  })

  it("handles reopen PR failure and error, plus drift and unknown op badges", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([
      { ...mockProposals[0], status: "closed" },
    ])
    vi.mocked(api.getProposalPreview).mockResolvedValue({
      ...mockPreview,
      resources: [
        {
          op: "Drift" as unknown as "Create",
          urn: "urn:pulumi:dev::app::pkg:Res::drifted",
          resource_type: "pkg:Res",
          name: "drifted",
          changed_fields: [],
        },
        {
          op: "noop" as unknown as "Create",
          urn: "urn:pulumi:dev::app::pkg:Res::unknown",
          resource_type: "pkg:Res",
          name: "unknown",
          changed_fields: [],
        },
      ],
    })
    vi.mocked(api.reopenProposal)
      .mockResolvedValueOnce({ success: false, message: "Cannot reopen" })
      .mockRejectedValueOnce(new Error("Reopen network error"))

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByText("Drift")).toBeInTheDocument()
      expect(screen.getByText("noop")).toBeInTheDocument()
    })

    const reopenBtn = screen.getByRole("button", { name: /Reopen PR/i })
    fireEvent.click(reopenBtn)
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Cannot reopen")
    })

    fireEvent.click(reopenBtn)
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Reopen network error"
      )
    })
  })

  it("handles getProposals rejection and status filter tab switches", async () => {
    vi.mocked(api.getProposals)
      .mockRejectedValueOnce(new Error("Proposals API down"))
      .mockResolvedValue([
        { ...mockProposals[0], id: "100", status: "closed" },
        { ...mockProposals[0], id: "101", status: "open" },
        { ...mockProposals[0], id: "102", status: "open" },
      ])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    render(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Proposals API down")
    })

    fireEvent.click(screen.getByRole("button", { name: /^merged$/i }))
    await waitFor(() => {
      expect(api.getProposals).toHaveBeenCalledWith(undefined, "merged")
      expect(screen.getByText("Pull Requests (3)")).toBeInTheDocument()
    })
  })
})
