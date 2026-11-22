import {
  render,
  screen,
  fireEvent,
  waitFor,
  within,
  act,
} from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import {
  PolicyStatusBanner,
  PreviewInspectorView,
} from "./PreviewInspectorView"
import * as api from "../lib/api"
import { ApiError } from "../lib/errors"
import { buildAuthz } from "../lib/authz"
import { renderWithAuthz } from "../test/render"
import { CrdWatchEvent, PreviewReport, Proposal } from "../types/api"

vi.mock("../lib/api", () => {
  // The view loads `getProposalListing`; most cases only care about the PR list, so the listing
  // delegates to the `getProposals` mock (no unavailable repositories).
  const getProposals = vi.fn()
  return {
    getProposals,
    getProposalListing: vi.fn(async (...args: unknown[]) => ({
      proposals: await getProposals(...args),
      unavailable: [],
    })),
    proposalKey: (ref: { scope: string; id: string }) =>
      `${ref.scope}/${ref.id}`,
    getProposalPreview: vi.fn(),
    mergeProposal: vi.fn(),
    rejectProposal: vi.fn(),
    reopenProposal: vi.fn(),
    createEventSourceStream: vi.fn(() => ({ close: vi.fn() })),
  }
})

const mockProposals: Proposal[] = [
  {
    id: "101",
    title: "Deploy Redis Cache",
    branch: "keel/tenant-acme/redis-101",
    author: "dev@acme.com",
    tenant: "acme",
    scope: "acme",
    repository: {
      label: "tenant-acme",
      fullName: "keel-tenants/infra-tenant-acme",
    },
    repoUrl: "https://git.internal/keel-tenants/infra-tenant-acme",
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

async function confirmReject() {
  fireEvent.click(screen.getByRole("button", { name: /Reject PR/i }))
  const dialog = await screen.findByRole("alertdialog")
  fireEvent.click(within(dialog).getByRole("button", { name: /Reject PR/i }))
}

describe("PreviewInspectorView component", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    window.history.replaceState({}, "", "/")
  })

  it("renders PR preview inspector and loads proposals and diff tree", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    renderWithAuthz(<PreviewInspectorView />)

    expect(screen.getByText("PR & Preview Inspector")).toBeInTheDocument()
    await waitFor(() => {
      expect(
        screen.getByText("PR #101: Deploy Redis Cache")
      ).toBeInTheDocument()
      expect(screen.getByText("redis-master")).toBeInTheDocument()
    })
    expect(screen.getByRole("link", { name: /Forge Link/i })).toHaveAttribute(
      "href",
      "https://git.internal/pr/101"
    )
    // Labeled (icon + text) action buttons.
    expect(screen.getByRole("button", { name: "Reject PR" })).toHaveTextContent(
      "Reject PR"
    )
    expect(screen.getByRole("button", { name: "Merge PR" })).toHaveTextContent(
      "Merge PR"
    )
    // No source reported: generic title, unknown-source badge, no policy card.
    expect(screen.getByTestId("preview-section-title")).toHaveTextContent(
      /^Preview$/
    )
    expect(screen.getByTestId("preview-source-badge")).toHaveTextContent(
      "Unknown source"
    )
    expect(screen.queryByTestId("policy-status-banner")).not.toBeInTheDocument()
  })

  it("merges PR successfully when Merge PR button is clicked", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.mergeProposal).mockResolvedValue({
      success: true,
      message: "Merged cleanly into main",
    })

    renderWithAuthz(<PreviewInspectorView />)

    await screen.findByText("PR #101: Deploy Redis Cache")
    fireEvent.click(screen.getByRole("button", { name: /Merge PR/i }))

    await waitFor(() => {
      expect(api.mergeProposal).toHaveBeenCalledWith({
        scope: "acme",
        id: "101",
      })
      expect(screen.getByRole("status")).toHaveTextContent(
        "Merged cleanly into main"
      )
      expect(screen.getByRole("button", { name: "Merged" })).toBeDisabled()
      expect(screen.getByRole("button", { name: /Reject PR/i })).toBeDisabled()
    })

    fireEvent.click(
      screen.getByRole("button", { name: /Dismiss notification/i })
    )
    expect(
      screen.queryByText("Merged cleanly into main")
    ).not.toBeInTheDocument()
  })

  it("asks for confirmation before rejecting and can be cancelled", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.rejectProposal).mockResolvedValue({ success: true })

    renderWithAuthz(<PreviewInspectorView />)
    await screen.findByText("PR #101: Deploy Redis Cache")

    fireEvent.click(screen.getByRole("button", { name: /Reject PR/i }))
    const dialog = await screen.findByRole("alertdialog")
    expect(dialog).toHaveTextContent("Reject PR #101?")
    expect(dialog).toHaveTextContent("keel/tenant-acme/redis-101")
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }))
    await waitFor(() =>
      expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument()
    )
    expect(api.rejectProposal).not.toHaveBeenCalled()

    await confirmReject()
    await waitFor(() => {
      expect(api.rejectProposal).toHaveBeenCalledWith({
        scope: "acme",
        id: "101",
      })
      expect(screen.getByRole("status")).toHaveTextContent(
        "PR #101 rejected and closed."
      )
      expect(screen.getByRole("button", { name: /Reopen PR/i })).toBeEnabled()
    })
  })

  it("handles merge failure and preview fetch error gracefully", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockRejectedValue(
      new Error("PKO preview runner unavailable")
    )

    renderWithAuthz(<PreviewInspectorView />)

    await screen.findByText("PKO preview runner unavailable")

    vi.mocked(api.mergeProposal).mockResolvedValue({
      success: false,
      message: "Merge conflicts detected",
    })
    fireEvent.click(screen.getByRole("button", { name: /Merge PR/i }))
    await screen.findByText("Merge conflicts detected")

    vi.mocked(api.rejectProposal).mockResolvedValue({
      success: true,
      message: "PR #101 rejected and closed.",
    })
    await confirmReject()
    await screen.findByText("PR #101 rejected and closed.")

    fireEvent.click(screen.getByRole("button", { name: /^merged$/i }))
    await waitFor(() => {
      expect(api.getProposals).toHaveBeenCalledWith(undefined, "merged")
    })
  })

  it("shows the correlation id for 5xx action errors", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.mergeProposal).mockRejectedValue(
      new ApiError(500, "Internal server error", { correlationId: "abc-123" })
    )

    renderWithAuthz(<PreviewInspectorView />)
    await screen.findByText("PR #101: Deploy Redis Cache")
    fireEvent.click(screen.getByRole("button", { name: /Merge PR/i }))

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Internal server error (reference: abc-123)"
      )
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

    renderWithAuthz(<PreviewInspectorView />)
    await waitFor(() => expect(api.getProposalPreview).toHaveBeenCalled())

    await confirmReject()
    await screen.findByText("Cannot reject merged PR")
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

    renderWithAuthz(<PreviewInspectorView />)

    const reopenBtn = await screen.findByRole("button", { name: /Reopen PR/i })
    fireEvent.click(reopenBtn)

    await waitFor(() => {
      expect(api.reopenProposal).toHaveBeenCalledWith({
        scope: "acme",
        id: "101",
      })
      expect(screen.getByText("PR #101 reopened.")).toBeInTheDocument()
      expect(screen.getByRole("button", { name: /Merge PR/i })).toBeEnabled()
      expect(screen.getByRole("button", { name: /Reject PR/i })).toBeEnabled()
    })
  })

  it("disables PR actions for users who are not admins of the PR tenant", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    const member = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["tenant-acme"],
    })
    const { unmount } = renderWithAuthz(<PreviewInspectorView />, member)
    await screen.findByText("PR #101: Deploy Redis Cache")
    expect(screen.getByRole("button", { name: /Merge PR/i })).toBeDisabled()
    expect(screen.getByRole("button", { name: /Reject PR/i })).toBeDisabled()
    expect(screen.getByTestId("pr-actions-permission-hint")).toHaveTextContent(
      'Requires cluster admin or tenant admin for "acme"'
    )
    unmount()

    const tenantAdmin = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["tenant-acme-admin"],
    })
    renderWithAuthz(<PreviewInspectorView />, tenantAdmin)
    await screen.findByText("PR #101: Deploy Redis Cache")
    expect(screen.getByRole("button", { name: /Merge PR/i })).toBeEnabled()
    expect(
      screen.queryByTestId("pr-actions-permission-hint")
    ).not.toBeInTheDocument()
  })

  it("disables actions without an authz provider (unauthenticated default)", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    render(<PreviewInspectorView />)
    await screen.findByText("PR #101: Deploy Redis Cache")
    expect(screen.getByRole("button", { name: /Merge PR/i })).toBeDisabled()
  })

  it("selects the deep-linked PR and reports user selection", async () => {
    const onSelect = vi.fn()
    vi.mocked(api.getProposals).mockResolvedValue([
      mockProposals[0],
      { ...mockProposals[0], id: "102", title: "Second PR" },
    ])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    const { rerender } = renderWithAuthz(
      <PreviewInspectorView
        selectedProposal={{ scope: "acme", id: "102" }}
        onSelectProposal={onSelect}
      />
    )
    await screen.findByText("PR #102: Second PR")
    expect(api.getProposalPreview).toHaveBeenCalledWith({
      scope: "acme",
      id: "102",
    })
    expect(onSelect).not.toHaveBeenCalled()

    fireEvent.click(screen.getByText("Deploy Redis Cache"))
    await screen.findByText("PR #101: Deploy Redis Cache")
    expect(onSelect).toHaveBeenCalledWith({ scope: "acme", id: "101" })

    // A new deep link supersedes the manual click.
    rerender(
      <PreviewInspectorView
        selectedProposal={{ scope: "acme", id: "102" }}
        onSelectProposal={onSelect}
      />
    )
    rerender(
      <PreviewInspectorView
        selectedProposal={{ scope: "acme", id: "101" }}
        onSelectProposal={onSelect}
      />
    )
    rerender(
      <PreviewInspectorView
        selectedProposal={{ scope: "acme", id: "102" }}
        onSelectProposal={onSelect}
      />
    )
    await screen.findByText("PR #102: Second PR")
  })

  it("lists every repository for a deep link outside the selected tenant", async () => {
    const platformPr: Proposal = {
      ...mockProposals[0],
      id: "42",
      title: "Add tenant globex",
      tenant: "management",
      scope: "management",
      repository: { label: "platform" },
    }
    const listing = vi.mocked(api.getProposalListing)
    const delegate = listing.getMockImplementation()
    listing.mockImplementation(async (tenant) => ({
      proposals: tenant
        ? [{ ...mockProposals[0], scope: tenant }]
        : [mockProposals[0], platformPr],
      unavailable: [],
    }))
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    renderWithAuthz(
      <PreviewInspectorView
        selectedTenant="default"
        selectedProposal={{ scope: "management", id: "42" }}
      />
    )

    await screen.findByText("PR #42: Add tenant globex")
    expect(api.getProposalListing).toHaveBeenLastCalledWith(undefined, "all")
    expect(screen.queryByTestId("pr-deeplink-missing")).not.toBeInTheDocument()
    expect(api.getProposalPreview).toHaveBeenCalledWith({
      scope: "management",
      id: "42",
    })
    if (delegate) listing.mockImplementation(delegate)
  })

  it("shows a notice when the deep-linked PR is not visible", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    renderWithAuthz(
      <PreviewInspectorView selectedProposal={{ scope: "acme", id: "999" }} />
    )

    expect(await screen.findByTestId("pr-deeplink-missing")).toHaveTextContent(
      "PR #999 (acme) not found or not visible"
    )
    await screen.findByText("PR #101: Deploy Redis Cache")
  })

  it("keys PRs by scope: equal ids in two repositories stay distinct", async () => {
    const globex: Proposal = {
      ...mockProposals[0],
      title: "Globex Podinfo",
      tenant: "globex",
      scope: "globex",
      repository: {
        label: "tenant-globex",
        fullName: "keel-tenants/infra-tenant-globex",
      },
    }
    vi.mocked(api.getProposals).mockResolvedValue([mockProposals[0], globex])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.mergeProposal).mockResolvedValue({ success: true })
    const onSelect = vi.fn()

    renderWithAuthz(
      <PreviewInspectorView
        selectedProposal={{ scope: "globex", id: "101" }}
        onSelectProposal={onSelect}
      />
    )
    await screen.findByText("PR #101: Globex Podinfo")
    expect(api.getProposalPreview).toHaveBeenCalledWith({
      scope: "globex",
      id: "101",
    })
    expect(screen.getAllByText("PR #101")).toHaveLength(2)

    fireEvent.click(screen.getByRole("button", { name: /Merge PR/i }))
    await waitFor(() =>
      expect(api.mergeProposal).toHaveBeenCalledWith({
        scope: "globex",
        id: "101",
      })
    )
    // Only the globex PR changed status.
    const list = screen.getByTestId("pr-list")
    const cards = within(list).getAllByRole("button")
    const statusOf = (title: string) =>
      cards.find((c) => c.textContent?.includes(title))?.textContent
    await waitFor(() => expect(statusOf("Globex Podinfo")).toMatch(/merged/))
    expect(statusOf("Deploy Redis Cache")).toMatch(/open/)

    fireEvent.click(within(list).getByText("Deploy Redis Cache"))
    expect(onSelect).toHaveBeenCalledWith({ scope: "acme", id: "101" })
    await screen.findByText("PR #101: Deploy Redis Cache")
  })

  it("shows the repository of each PR, forge links and unavailable repositories", async () => {
    vi.mocked(api.getProposalListing).mockResolvedValueOnce({
      proposals: mockProposals,
      unavailable: [
        {
          scope: "globex",
          repository: {
            label: "tenant-globex",
            fullName: "keel-tenants/infra-tenant-globex",
          },
          reason: "forge unavailable or rate limited",
        },
      ],
    })
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    renderWithAuthz(<PreviewInspectorView selectedTenant="acme" />)
    await screen.findByText("PR #101: Deploy Redis Cache")
    expect(api.getProposalListing).toHaveBeenCalledWith("acme", "all")
    expect(
      screen.getByText("keel-tenants/infra-tenant-acme", {
        selector: "span[title]",
      })
    ).toBeInTheDocument()
    expect(
      screen.getByRole("link", { name: /Repository Link/i })
    ).toHaveAttribute(
      "href",
      "https://git.internal/keel-tenants/infra-tenant-acme"
    )
    expect(
      within(screen.getByTestId("pr-list")).getByTestId("pr-repository")
    ).toHaveTextContent("keel-tenants/infra-tenant-acme")
    expect(screen.getByTestId("pr-unavailable-repositories")).toHaveTextContent(
      /keel-tenants\/infra-tenant-globex.*forge unavailable/
    )
  })

  it("requires a cluster admin for platform-repository PRs", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([
      {
        ...mockProposals[0],
        id: "9",
        title: "Create tenant acme",
        scope: "management",
        repository: { label: "platform" },
        repoUrl: undefined,
        prUrl: "local:///tmp/remote.git/.keel/proposals/9.json",
      },
    ])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    const tenantAdmin = buildAuthz({
      authenticated: true,
      phase: "oidc-active",
      groups: ["tenant-acme-admin"],
    })
    renderWithAuthz(<PreviewInspectorView />, tenantAdmin)
    await screen.findByText("PR #9: Create tenant acme")
    expect(screen.getByRole("button", { name: /Merge PR/i })).toBeDisabled()
    expect(screen.getByTestId("pr-actions-permission-hint")).toHaveTextContent(
      "Requires cluster admin (platform repository)"
    )
    // Non-http(s) forge URLs (Local Git provider) are never rendered as links.
    expect(screen.queryByRole("link", { name: /Forge Link/i })).toBeNull()
    expect(
      screen.getByText("platform repository", { selector: "span[title]" })
    ).toBeInTheDocument()
  })

  it("renders clean empty state when getProposals returns empty array in non-demo mode", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([])

    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(
        screen.getByText(
          "No open proposals found — create a proposal from the Catalog."
        )
      ).toBeInTheDocument()
      expect(
        screen.getByText(
          "No proposal selected. Create a proposal from the Catalog to inspect its preview diff."
        )
      ).toBeInTheDocument()
    })
  })

  it("renders demo fixtures without calling the live API when demo=true", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByText(/PR #\d+:/)).toBeInTheDocument()
    })
    expect(api.getProposals).not.toHaveBeenCalled()
    expect(api.getProposalPreview).not.toHaveBeenCalled()
    expect(screen.queryByRole("alert")).not.toBeInTheDocument()
    window.history.replaceState({}, "", "/")
  })

  it("handles keyboard selection on PR items and string error on merge failure", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.mergeProposal).mockRejectedValue("Server timeout")

    renderWithAuthz(<PreviewInspectorView />)
    await screen.findByText("PR #101: Deploy Redis Cache")

    const prCard = screen.getByText("PR #101").closest('[role="button"]')!
    fireEvent.keyDown(prCard, { key: "Enter" })
    fireEvent.keyDown(prCard, { key: " " })

    fireEvent.click(screen.getByRole("button", { name: /Merge PR/i }))
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to merge PR proposal"
      )
    })
  })

  it("handles preview fetch string rejection", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockRejectedValue("Preview offline")

    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to load preview diff"
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

    renderWithAuthz(<PreviewInspectorView />)

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

    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent("Proposals API down")
    })

    const mergedTab = screen.getByRole("button", { name: /^merged$/i })
    fireEvent.click(mergedTab)
    expect(mergedTab).toHaveAttribute("aria-pressed", "true")
    await waitFor(() => {
      expect(api.getProposals).toHaveBeenCalledWith(undefined, "merged")
      expect(screen.getByText("Pull Requests (3)")).toBeInTheDocument()
    })
  })

  it("supports keyboard selection in PR list and handles reject failure branches", async () => {
    vi.mocked(api.getProposals).mockResolvedValue([
      mockProposals[0],
      {
        ...mockProposals[0],
        id: "102",
        title: "Second PR",
        branch: "keel/second",
      },
    ])
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)
    vi.mocked(api.rejectProposal)
      .mockResolvedValueOnce({ success: false, message: "" })
      .mockRejectedValueOnce("reject string error")

    renderWithAuthz(<PreviewInspectorView />)

    const secondPr = await screen.findByText("Second PR")
    const card = secondPr.closest("[role='button']") as HTMLElement
    fireEvent.keyDown(card, { key: "Enter" })
    fireEvent.keyDown(card, { key: " " })

    await waitFor(() => {
      expect(api.getProposalPreview).toHaveBeenCalledWith({
        scope: "acme",
        id: "102",
      })
    })

    await confirmReject()
    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to reject PR proposal"
      )
    })

    await confirmReject()
    await waitFor(() => {
      expect(api.rejectProposal).toHaveBeenCalledTimes(2)
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to reject PR proposal"
      )
    })
  })

  it("shows the PKO run phase and refreshes the preview on PKO preview SSE events", async () => {
    let onEvent: ((e: CrdWatchEvent) => void) | undefined
    vi.mocked(api.createEventSourceStream).mockImplementation((ev) => {
      onEvent = ev
      return { close: vi.fn() } as unknown as EventSource
    })
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview)
      .mockResolvedValueOnce({
        ...mockPreview,
        source: "pko",
        run: { phase: "running", message: "pulumi preview in progress" },
      })
      .mockResolvedValue({
        ...mockPreview,
        source: "pko",
        run: { phase: "succeeded", stack: "keel-previews/pr-101" },
      })

    renderWithAuthz(<PreviewInspectorView />)

    expect(await screen.findByTestId("preview-run-status")).toHaveAttribute(
      "data-phase",
      "running"
    )
    await waitFor(() => expect(onEvent).toBeDefined())
    act(() =>
      onEvent!({
        kind: "Stack",
        namespace: "keel-previews",
        name: "pr-101",
        status: "Ready",
        message: "PKO preview succeeded",
        timestamp: "now",
      })
    )
    await waitFor(() => {
      expect(screen.getByTestId("preview-run-status")).toHaveAttribute(
        "data-phase",
        "succeeded"
      )
    })
    expect(api.getProposalPreview).toHaveBeenCalledTimes(2)
    expect(screen.getByTestId("preview-run-status")).toHaveTextContent(
      "keel-previews/pr-101"
    )
    expect(screen.getByTestId("policy-status-banner")).toBeInTheDocument()
  })

  it("hides the policy card when the PKO preview was not executed", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue({
      ...mockPreview,
      source: "pko",
      run: { phase: "notExecuted", message: "no infrastructure changes" },
    })

    renderWithAuthz(<PreviewInspectorView />)

    expect(await screen.findByTestId("preview-run-status")).toHaveTextContent(
      "Not executed: no infrastructure changes"
    )
    expect(screen.queryByTestId("policy-status-banner")).not.toBeInTheDocument()
    expect(api.createEventSourceStream).not.toHaveBeenCalled()
  })

  it("filters PRs via client-side search query by title, branch, ID, tenant, or author", async () => {
    const multiPRs: Proposal[] = [
      {
        ...mockProposals[0],
        id: "101",
        title: "Deploy Redis Cache",
        branch: "branch-redis",
        tenant: "tenant-a",
        scope: "a",
        repository: { label: "tenant-a" },
        author: "alice",
      },
      {
        ...mockProposals[0],
        id: "102",
        title: "Scale Kafka",
        branch: "branch-kafka",
        tenant: "tenant-b",
        scope: "b",
        repository: { label: "tenant-b" },
        author: "bob",
      },
      {
        ...mockProposals[0],
        id: "103",
        title: "Update Postgres",
        branch: "branch-pg",
        tenant: "tenant-a",
        scope: "a",
        repository: { label: "tenant-a" },
        author: "charlie",
      },
    ]
    vi.mocked(api.getProposals).mockResolvedValue(multiPRs)
    vi.mocked(api.getProposalPreview).mockResolvedValue(mockPreview)

    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByText("Deploy Redis Cache")).toBeInTheDocument()
      expect(screen.getByText("Scale Kafka")).toBeInTheDocument()
      expect(screen.getByText("Update Postgres")).toBeInTheDocument()
    })

    const searchInput = screen.getByTestId("pr-search-input")

    // Filter by title
    fireEvent.change(searchInput, { target: { value: "redis" } })
    expect(screen.getByText("Deploy Redis Cache")).toBeInTheDocument()
    expect(screen.queryByText("Scale Kafka")).toBeNull()

    // Filter by tenant
    fireEvent.change(searchInput, { target: { value: "tenant-a" } })
    expect(screen.getByText("Deploy Redis Cache")).toBeInTheDocument()
    expect(screen.getByText("Update Postgres")).toBeInTheDocument()
    expect(screen.queryByText("Scale Kafka")).toBeNull()

    // Filter by ID
    fireEvent.change(searchInput, { target: { value: "102" } })
    expect(screen.getByText("Scale Kafka")).toBeInTheDocument()
    expect(screen.queryByText("Update Postgres")).toBeNull()

    // Empty search state
    fireEvent.change(searchInput, { target: { value: "non-existent-xyz" } })
    expect(
      screen.getByText("No pull requests matching your search.")
    ).toBeInTheDocument()
    expect(screen.queryByText("Scale Kafka")).toBeNull()

    // Repository (scope) filter narrows the aggregated listing.
    fireEvent.change(searchInput, { target: { value: "" } })
    fireEvent.change(screen.getByTestId("pr-scope-filter"), {
      target: { value: "b" },
    })
    expect(screen.getByText("Scale Kafka")).toBeInTheDocument()
    expect(screen.queryByText("Update Postgres")).toBeNull()
  })

  it("renders policy banner from status instead of hardcoded pass", () => {
    const { rerender } = render(<PolicyStatusBanner />)
    const banner = screen.getByTestId("policy-status-banner")
    expect(banner).toHaveAttribute("data-status", "unknown")
    expect(
      screen.getByText("Policy evaluation not available for this preview")
    ).toBeInTheDocument()
    expect(screen.getByText("not evaluated")).toBeInTheDocument()
    expect(screen.queryByText(/PSS passed/)).not.toBeInTheDocument()

    rerender(<PolicyStatusBanner status="failed" />)
    expect(banner).toHaveAttribute("data-status", "failed")
    expect(screen.getByText("failed")).toHaveClass("text-red-400")

    rerender(<PolicyStatusBanner status="warning" />)
    expect(screen.getByText("warning")).toHaveClass("text-amber-400")

    rerender(<PolicyStatusBanner status="passed" />)
    expect(screen.getByText("passed")).toHaveClass("text-emerald-400")
  })

  it("shows simulated diff badge and hides the policy card for synthesized previews", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue({
      ...mockPreview,
      policyStatus: "passed",
      source: "synthesized",
    })

    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByTestId("simulated-preview-badge")).toHaveTextContent(
        "Simulated diff — PKO preview not executed"
      )
    })
    expect(screen.getByTestId("preview-source-badge")).toHaveTextContent(
      "Synthesized (not executed)"
    )
    expect(screen.getByTestId("preview-section-title")).toHaveTextContent(
      /^Preview$/
    )
    expect(screen.queryByTestId("policy-status-banner")).not.toBeInTheDocument()
  })

  it("omits simulated badge for real PKO previews", async () => {
    vi.mocked(api.getProposals).mockResolvedValue(mockProposals)
    vi.mocked(api.getProposalPreview).mockResolvedValue({
      ...mockPreview,
      source: "pko",
    })

    renderWithAuthz(<PreviewInspectorView />)

    await waitFor(() => {
      expect(screen.getByTestId("policy-status-banner")).toHaveAttribute(
        "data-status",
        "passed"
      )
    })
    expect(
      screen.queryByTestId("simulated-preview-badge")
    ).not.toBeInTheDocument()
    expect(screen.getByTestId("preview-section-title")).toHaveTextContent(
      "PKO Preview"
    )
    expect(screen.getByTestId("preview-source-badge")).toHaveTextContent(
      "PKO preview"
    )
  })
})
