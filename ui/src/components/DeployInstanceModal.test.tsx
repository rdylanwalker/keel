import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import {
  DeployInstanceModal,
  extractSchemaDefaults,
} from "./DeployInstanceModal"
import * as api from "../lib/api"
import { CatalogItem } from "../types/api"

vi.mock("../lib/api", () => ({
  createProposal: vi.fn(),
  fetchCatalogSchema: vi.fn(),
}))

const sampleAppItem: CatalogItem = {
  id: "podinfo",
  name: "Podinfo App",
  description: "Test microservice",
  category: "apps",
  type: "app",
  runtime: "helm",
  repoUrl: "https://stefanprodan.github.io/podinfo",
  chart: "podinfo",
  version: "6.5.0",
  schema: {
    type: "object",
    required: ["replicaCount"],
    properties: {
      replicaCount: {
        type: "integer",
        title: "Replicas",
        default: 1,
      },
      nested: {
        type: "object",
        properties: {
          enabled: { type: "boolean", default: true },
        },
      },
    },
  },
}

describe("DeployInstanceModal component", () => {
  const onClose = vi.fn()
  const onProposalCreated = vi.fn()
  const onCatalogItemUpdated = vi.fn()

  beforeEach(() => {
    vi.clearAllMocks()
  })

  it("extractSchemaDefaults resolves nested defaults", () => {
    const defaults = extractSchemaDefaults(
      sampleAppItem.schema as Record<string, unknown>,
      { replicaCount: 3 }
    )
    expect(defaults).toEqual({
      replicaCount: 3,
      nested: { enabled: true },
    })
  })

  it("renders loading and error states in edit mode", () => {
    const { rerender } = render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={null}
        tenant="acme"
        isEditMode={true}
        loadingConfig={true}
      />
    )
    expect(
      screen.getByText(/Loading instance configuration/i)
    ).toBeInTheDocument()

    rerender(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={null}
        tenant="acme"
        isEditMode={true}
        loadingConfig={false}
        configError="Failed to load instance"
      />
    )
    expect(screen.getByText("Failed to load instance")).toBeInTheDocument()
  })

  it("submits proposal and syncs upstream Helm schema", async () => {
    vi.mocked(api.fetchCatalogSchema).mockResolvedValueOnce({
      schema: {
        type: "object",
        properties: {
          replicaCount: { type: "integer", default: 2 },
          servicePort: { type: "integer", default: 9898 },
        },
      },
      source: "values.yaml",
    })
    vi.mocked(api.createProposal).mockResolvedValueOnce({
      id: "42",
      title: "deploy podinfo",
      branch: "keel/acme/podinfo",
      status: "open",
      author: "admin@keel.local",
      tenant: "acme",
      createdAt: "2026-11-15T12:00:00Z",
    })

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={sampleAppItem}
        tenant="acme"
        stacks={[
          {
            id: "dev-kind",
            name: "dev-kind",
            namespace: "tenant-acme",
            tenant: "acme",
            provider: "kind",
            type: "workload",
            status: "synced",
          },
        ]}
        onProposalCreated={onProposalCreated}
        onCatalogItemUpdated={onCatalogItemUpdated}
      />
    )

    // Sync upstream schema
    fireEvent.click(screen.getByRole("button", { name: /Sync Upstream/i }))
    await waitFor(() => {
      expect(api.fetchCatalogSchema).toHaveBeenCalled()
      expect(onCatalogItemUpdated).toHaveBeenCalled()
    })

    // Toggle fullscreen
    fireEvent.click(screen.getByRole("button", { name: /Enter full screen/i }))
    expect(
      screen.getByRole("button", { name: /Exit full screen/i })
    ).toBeInTheDocument()

    // Submit proposal
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )
    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalled()
      expect(onProposalCreated).toHaveBeenCalled()
      expect(onClose).toHaveBeenCalled()
    })
  })

  it("renders Enable Flagger Canary toggle when flaggerSupported is true and submits canary: true", async () => {
    vi.mocked(api.createProposal).mockResolvedValueOnce({
      id: "43",
      title: "deploy podinfo canary",
      branch: "keel/acme/app-podinfo-43",
      author: "tester",
      tenant: "acme",
      status: "open",
      createdAt: "just now",
    })

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={{
          ...sampleAppItem,
          flaggerSupported: true,
          defaultCanary: false,
        }}
        tenant="acme"
        onProposalCreated={onProposalCreated}
      />
    )

    const canaryCheckbox = screen.getByLabelText(/Enable Flagger Canary/i)
    expect(canaryCheckbox).not.toBeChecked()
    fireEvent.click(canaryCheckbox)
    expect(canaryCheckbox).toBeChecked()

    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )
    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({
          catalogId: "podinfo",
          tenant: "acme",
          canary: true,
        })
      )
    })
  })
})
