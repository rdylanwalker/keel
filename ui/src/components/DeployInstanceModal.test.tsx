import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { DeployInstanceModal } from "./DeployInstanceModal"
import { extractSchemaDefaults } from "../lib/schemaBuilder"
import * as api from "../lib/api"
import { CatalogItem } from "../types/api"

vi.mock("../lib/api", () => ({
  createProposal: vi.fn(),
  fetchCatalogSchema: vi.fn(),
  upgradeManagementAddon: vi.fn(),
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

  it("renders bundle graph, tenant/cluster/namespace controls, and custom kubeconfig secret", async () => {
    const onTenantChange = vi.fn()
    vi.mocked(api.createProposal).mockResolvedValueOnce({
      id: "44",
      title: "deploy bundle",
      branch: "keel/beta/bundle-44",
      author: "tester",
      tenant: "beta",
      status: "open",
      createdAt: "just now",
    })

    const { rerender } = render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={{
          ...sampleAppItem,
          id: "full-stack",
          name: "Full Stack",
          type: "bundle",
          bundleItems: [
            { id: "vpc", kind: "infra", catalogRef: "aws-vpc" },
            {
              id: "web",
              kind: "app",
              catalogRef: "podinfo",
              canary: true,
              dependsOn: ["vpc"],
            },
          ],
        }}
        tenant="acme"
        tenants={[
          { id: "acme", namespace: "tenant-acme" },
          { id: "beta", namespace: "tenant-beta" },
        ]}
        onTenantChange={onTenantChange}
        onProposalCreated={onProposalCreated}
      />
    )

    expect(screen.getByText(/Bundle Execution Graph/i)).toBeInTheDocument()
    expect(screen.getByText("← vpc")).toBeInTheDocument()

    fireEvent.change(screen.getByLabelText(/Target Tenant/i), {
      target: { value: "beta" },
    })
    expect(onTenantChange).toHaveBeenCalledWith("beta")

    rerender(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={sampleAppItem}
        tenant="beta"
        tenants={[
          { id: "acme", namespace: "tenant-acme" },
          { id: "beta", namespace: "tenant-beta" },
        ]}
        onTenantChange={onTenantChange}
        onProposalCreated={onProposalCreated}
      />
    )

    fireEvent.change(screen.getByLabelText(/Instance Name/i), {
      target: { value: "podinfo-west" },
    })
    fireEvent.change(screen.getByLabelText(/Chart Version/i), {
      target: { value: "6.6.0" },
    })
    fireEvent.change(screen.getByLabelText(/Target Cluster/i), {
      target: { value: "__custom__" },
    })
    fireEvent.change(screen.getByLabelText(/Custom kubeconfig Secret name/i), {
      target: { value: "west-kubeconfig" },
    })
    fireEvent.change(screen.getByLabelText(/Target Namespace/i), {
      target: { value: "web-prod" },
    })

    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )

    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({
          name: "podinfo-west",
          version: "6.6.0",
          targetCluster: "west-kubeconfig",
          targetNamespace: "web-prod",
        })
      )
    })
  })

  it("supports managementAddon mode with chart version, Sync Upstream Schema, and upgradeManagementAddon submit", async () => {
    vi.mocked(api.fetchCatalogSchema).mockResolvedValueOnce({
      schema: {
        type: "object",
        properties: {
          replicaCount: { type: "integer", default: 3 },
        },
      },
      source: "values.yaml",
    })
    vi.mocked(api.upgradeManagementAddon).mockResolvedValueOnce({
      id: "88",
      title: "Upgrade kyverno",
      branch: "keel/mgmt/upgrade-kyverno",
      status: "open",
      author: "admin@keel.local",
      tenant: "keel-system",
      createdAt: "2026-11-16T12:00:00Z",
    })

    const mgmtAddon: CatalogItem = {
      id: "kyverno",
      name: "Kyverno",
      description: "Policy engine",
      category: "Management Addons",
      type: "app",
      repoUrl: "https://kyverno.github.io/kyverno/",
      chart: "kyverno",
      version: "3.3.4",
      flaggerSupported: true,
      schema: {
        type: "object",
        properties: {
          replicaCount: { type: "integer", default: 1 },
        },
      },
    }

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={mgmtAddon}
        managementAddon={mgmtAddon}
        tenant="keel-system"
        tenants={[{ id: "acme", namespace: "tenant-acme" }]}
        onProposalCreated={onProposalCreated}
      />
    )

    // Tenant selector, Instance Name, and Canary toggle should be hidden
    expect(screen.queryByLabelText(/Target Tenant/i)).not.toBeInTheDocument()
    expect(screen.queryByLabelText(/Instance Name/i)).not.toBeInTheDocument()
    expect(
      screen.queryByLabelText(/Enable Flagger Canary/i)
    ).not.toBeInTheDocument()

    // Chart Version input is pre-populated and editable
    const versionInput = screen.getByLabelText(
      /Chart Version/i
    ) as HTMLInputElement
    expect(versionInput.value).toBe("3.3.4")
    fireEvent.change(versionInput, { target: { value: "3.3.5" } })

    // Sync Upstream Schema button is available and works
    fireEvent.click(screen.getByRole("button", { name: /Sync Upstream/i }))
    await waitFor(() => {
      expect(api.fetchCatalogSchema).toHaveBeenCalledWith({
        repoUrl: "https://kyverno.github.io/kyverno/",
        chart: "kyverno",
        version: "3.3.4",
      })
    })

    // Submit triggers upgradeManagementAddon
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )
    await waitFor(() => {
      expect(api.upgradeManagementAddon).toHaveBeenCalledWith("kyverno", {
        version: "3.3.5",
        values: { replicaCount: 1 },
      })
      expect(onProposalCreated).toHaveBeenCalledWith(
        expect.objectContaining({ id: "88" })
      )
      expect(onClose).toHaveBeenCalled()
    })
  })

  it("renders Action Engine selector for raw manifest catalog apps and passes actionEngine in proposal payload", async () => {
    vi.mocked(api.createProposal).mockResolvedValueOnce({
      id: "99",
      title: "Deploy custom-manifest",
      branch: "keel/acme/custom-manifest-99",
      author: "user",
      tenant: "acme",
      status: "open",
      createdAt: "just now",
    })

    const manifestItem: CatalogItem = {
      id: "custom-manifest",
      name: "Custom K8s Manifest",
      description: "Direct K8s manifest app",
      type: "app",
      category: "Applications",
      version: "1.0.0",
      runtime: "manifest",
      defaultEngine: "flux",
      schema: {
        type: "object",
        properties: {
          appName: { type: "string", default: "my-app" },
        },
      },
    }

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={manifestItem}
        tenant="acme"
        onProposalCreated={onProposalCreated}
      />
    )

    // Action Engine selector is present and defaulted to flux
    const engineSelect = screen.getByTestId("deploy-action-engine-select")
    expect(engineSelect).toHaveValue("flux")

    // Change to pulumi
    fireEvent.change(engineSelect, { target: { value: "pulumi" } })
    expect(engineSelect).toHaveValue("pulumi")

    // Submit
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )

    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({
          catalogId: "custom-manifest",
          tenant: "acme",
          actionEngine: "pulumi",
        })
      )
      expect(onProposalCreated).toHaveBeenCalled()
      expect(onClose).toHaveBeenCalled()
    })
  })

  it("supports initialConfig with actionEngine and switching between flux and pulumi", async () => {
    vi.mocked(api.createProposal).mockResolvedValueOnce({
      id: "100",
      title: "Deploy manifest app",
      branch: "keel/acme/manifest-100",
      author: "user",
      tenant: "acme",
      status: "open",
      createdAt: "just now",
    })

    const manifestItem: CatalogItem = {
      id: "manifest-app",
      name: "Manifest App",
      description: "Direct manifest app",
      type: "app",
      category: "Applications",
      version: "1.0.0",
      runtime: "manifest",
      defaultEngine: "pulumi",
      schema: { type: "object", properties: {} },
    }

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={vi.fn()}
        catalogItem={manifestItem}
        initialConfig={{
          actionEngine: "pulumi",
          name: "manifest-app",
          tenant: "acme",
        }}
        tenant="acme"
        onProposalCreated={vi.fn()}
      />
    )

    const engineSelect = screen.getByTestId("deploy-action-engine-select")
    expect(engineSelect).toHaveValue("pulumi")

    // Switch to flux
    fireEvent.change(engineSelect, { target: { value: "flux" } })
    expect(engineSelect).toHaveValue("flux")

    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )

    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({
          catalogId: "manifest-app",
          actionEngine: "flux",
        })
      )
    })
  })

  it("handles sync upstream schema failure gracefully", async () => {
    vi.mocked(api.fetchCatalogSchema).mockRejectedValueOnce(
      new Error("Schema fetch failed")
    )

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={vi.fn()}
        catalogItem={sampleAppItem}
        tenant="acme"
      />
    )

    fireEvent.click(screen.getByRole("button", { name: /Sync Upstream/i }))
    expect(await screen.findByText("Schema fetch failed")).toBeInTheDocument()
  })

  it("filters stacks in Target Cluster dropdown to only include 'cluster' resource kinds", async () => {
    const mixedStacks = [
      {
        id: "kind-stack",
        name: "dev-kind",
        tenant: "acme",
        provider: "kind",
        type: "workload",
        status: "synced",
      },
      {
        id: "aws-eks",
        name: "aws-eks-cluster",
        tenant: "acme",
        provider: "aws",
        type: "cloud",
        status: "synced",
      },
      {
        id: "vm-stack",
        name: "aws-vm",
        tenant: "acme",
        provider: "aws",
        type: "workload",
        status: "synced",
      },
      {
        id: "service-stack",
        name: "postgres-managed",
        tenant: "acme",
        provider: "kubernetes",
        type: "workload",
        status: "synced",
      },
      {
        id: "mgmt-stack",
        name: "management-cluster-core",
        tenant: "system",
        provider: "kind",
        type: "management",
        status: "synced",
      },
    ]

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={vi.fn()}
        catalogItem={sampleAppItem}
        tenant="acme"
        stacks={mixedStacks as import("../types/api").StackSummary[]}
      />
    )

    const clusterSelect = screen.getByLabelText(/Target Cluster/i)
    fireEvent.click(clusterSelect)

    // Should include:
    expect(
      screen.getByRole("option", { name: /dev-kind/i })
    ).toBeInTheDocument()
    expect(
      screen.getByRole("option", { name: /aws-eks-cluster/i })
    ).toBeInTheDocument()

    // Should exclude:
    expect(screen.queryByRole("option", { name: /aws-vm/i })).toBeNull()
    expect(
      screen.queryByRole("option", { name: /postgres-managed/i })
    ).toBeNull()
    expect(
      screen.queryByRole("option", { name: /management-cluster-core/i })
    ).toBeNull()
  })
})
