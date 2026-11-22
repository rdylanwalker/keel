import { render, screen, fireEvent, waitFor } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { DeployInstanceModal } from "./DeployInstanceModal"
import { extractSchemaDefaults } from "../lib/schemaBuilder"
import * as api from "../lib/api"
import { CatalogItem, StackSummary } from "../types/api"

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
      tenant: "acme",
      scope: "acme",
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
            resourceKind: "cluster",
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
      tenant: "acme",
      scope: "acme",
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
      tenant: "beta",
      scope: "beta",
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
      tenant: "keel-system",
      scope: "management",
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
      tenant: "acme",
      scope: "acme",
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
      tenant: "acme",
      scope: "acme",
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
        resourceKind: "cluster",
      },
      {
        id: "aws-eks",
        name: "aws-eks-cluster",
        tenant: "acme",
        provider: "aws",
        type: "cloud",
        status: "synced",
        resourceKind: "cluster",
      },
      {
        id: "vm-stack",
        name: "aws-vm",
        tenant: "acme",
        provider: "aws",
        type: "workload",
        status: "synced",
        resourceKind: "vm",
      },
      {
        id: "service-stack",
        name: "postgres-managed",
        tenant: "acme",
        provider: "kubernetes",
        type: "workload",
        status: "synced",
        resourceKind: "service",
      },
      {
        id: "mgmt-stack",
        name: "management-cluster-core",
        tenant: "system",
        provider: "kind",
        type: "management",
        status: "synced",
        resourceKind: "cluster",
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

  it("passes catalog uiSchema, renders writeOnly fields as password inputs, and masks secrets in the values.yaml preview", () => {
    const secretItem: CatalogItem = {
      ...sampleAppItem,
      id: "secret-app",
      schema: {
        type: "object",
        properties: {
          adminUser: { type: "string", title: "Admin User", default: "root" },
          apiToken: {
            type: "string",
            title: "API Token",
            writeOnly: true,
            default: "super-secret-token",
          },
          dbPassword: {
            type: "string",
            title: "DB Password",
            format: "password",
            default: "hunter2",
          },
        },
      },
      uiSchema: {
        adminUser: { "ui:help": "Catalog provided help text" },
      },
    }

    render(
      <DeployInstanceModal
        isOpen={true}
        onClose={onClose}
        catalogItem={secretItem}
        tenant="acme"
      />
    )

    expect(screen.getByText("Catalog provided help text")).toBeInTheDocument()
    expect(screen.getByLabelText(/API Token/)).toHaveAttribute(
      "type",
      "password"
    )
    expect(screen.getByLabelText(/DB Password/)).toHaveAttribute(
      "type",
      "password"
    )

    const preview = screen.getByLabelText("values.yaml preview")
    expect(preview.textContent).toContain("adminUser: root")
    expect(preview.textContent).toContain('apiToken: "********"')
    expect(preview.textContent).toContain('dbPassword: "********"')
    expect(preview.textContent).not.toContain("super-secret-token")
    expect(preview.textContent).not.toContain("hunter2")
  })
})

describe("DeployInstanceModal validation, secrets and server errors", () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  const secretItem: CatalogItem = {
    id: "svc",
    name: "Secret Service",
    description: "Needs a token",
    category: "apps",
    type: "app",
    runtime: "helm",
    version: "1.0.0",
    schema: {
      type: "object",
      required: ["apiToken", "host"],
      properties: {
        host: { type: "string", title: "Host" },
        apiToken: {
          type: "string",
          title: "API Secret Token",
          writeOnly: true,
        },
      },
    },
  }

  it("requires secrets on create but keeps existing secrets in edit mode", async () => {
    vi.mocked(api.createProposal).mockResolvedValue({
      id: "1",
      title: "t",
      branch: "b",
      tenant: "default",
      scope: "default",
      status: "open",
      createdAt: "now",
    })
    const { unmount } = render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={secretItem}
        tenant="default"
      />
    )
    fireEvent.change(screen.getByLabelText(/^Host/), {
      target: { value: "h" },
    })
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )
    expect(
      await screen.findByText("API Secret Token is required")
    ).toBeInTheDocument()
    expect(api.createProposal).not.toHaveBeenCalled()
    unmount()

    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={secretItem}
        tenant="default"
        isEditMode
        initialConfig={{ name: "svc", values: { host: "h" } }}
      />
    )
    const token = screen.getByLabelText(/API Secret Token/)
    expect(token).toHaveAttribute(
      "placeholder",
      "Leave blank to keep existing secret"
    )
    expect(token).toHaveAttribute("type", "password")
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )
    await waitFor(() => expect(api.createProposal).toHaveBeenCalled())
    expect(vi.mocked(api.createProposal).mock.calls[0][0].values).toEqual({
      host: "h",
    })
  })

  const manifestItem: CatalogItem = {
    id: "raw",
    name: "Raw App",
    description: "manifest",
    category: "apps",
    type: "app",
    runtime: "manifest",
    version: "1",
    manifestsYaml:
      "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ${name}\ndata:\n  image: repo/${image}:1\n",
    schema: {
      type: "object",
      properties: {
        name: { type: "string", title: "Name" },
        image: { type: "string", title: "Image" },
      },
    },
  }

  it("blocks manifest values the server would reject inside a scalar", async () => {
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={manifestItem}
        tenant="default"
        initialConfig={{ values: { name: "a: b # ok", image: "x#y" } }}
      />
    )
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )
    expect(
      (await screen.findAllByText(/cannot contain "#"/)).length
    ).toBeGreaterThan(0)
    expect(api.createProposal).not.toHaveBeenCalled()
  })

  it("maps server validation details and manifest errors to fields, shows correlation ids", async () => {
    const { ApiError } = await import("../lib/errors")
    vi.mocked(api.createProposal)
      .mockRejectedValueOnce(
        new ApiError(400, "Schema validation failed", {
          details: [{ fieldPath: "/host", message: "host is not reachable" }],
        })
      )
      .mockRejectedValueOnce(
        new ApiError(
          400,
          "Manifest variable 'image' contains YAML-structural characters"
        )
      )
      .mockRejectedValueOnce(
        new ApiError(500, "Internal server error", { correlationId: "abc-123" })
      )
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={{
          ...manifestItem,
          manifestsYaml: undefined,
          schema: {
            type: "object",
            properties: {
              host: { type: "string", title: "Host" },
              image: { type: "string", title: "Image" },
            },
          },
        }}
        tenant="default"
        initialConfig={{ values: { host: "h", image: "i" } }}
      />
    )
    const submit = () =>
      fireEvent.click(
        screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
      )
    submit()
    expect(await screen.findByText("host is not reachable")).toBeInTheDocument()
    expect(screen.getByText("Schema validation failed")).toBeInTheDocument()
    submit()
    expect(
      await screen.findByText("contains YAML-structural characters")
    ).toBeInTheDocument()
    submit()
    expect(
      await screen.findByText(/Internal server error \(reference: abc-123\)/)
    ).toBeInTheDocument()
  })

  it("closes on Escape via the Shadcn Dialog", async () => {
    const onClose = vi.fn()
    render(
      <DeployInstanceModal
        isOpen
        onClose={onClose}
        catalogItem={manifestItem}
        tenant="default"
      />
    )
    screen.getByRole("dialog", { name: /Create Raw App/ })
    fireEvent.keyDown(document.activeElement ?? document.body, {
      key: "Escape",
    })
    await waitFor(() => expect(onClose).toHaveBeenCalled())
  })
})

describe("DeployInstanceModal cluster-scoped placement", () => {
  const clusterScopedItem: CatalogItem = {
    ...sampleAppItem,
    id: "cert-manager",
    name: "cert-manager",
    placementScope: "cluster",
  }
  const kindStack: StackSummary = {
    id: "dev-kind",
    name: "dev-kind",
    namespace: "tenant-acme",
    tenant: "acme",
    provider: "kind",
    type: "workload",
    status: "synced",
    resourceKind: "cluster",
  }
  const submit = () =>
    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )

  beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(api.createProposal).mockResolvedValue({
      id: "77",
      title: "deploy cert-manager",
      branch: "keel/acme/cert-manager",
      tenant: "acme",
      scope: "acme",
      status: "open",
      createdAt: "just now",
    })
  })

  it("hides the management cluster, shows a hint, and blocks submit until a workload cluster is chosen", async () => {
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={clusterScopedItem}
        tenant="acme"
      />
    )
    expect(
      screen.queryByRole("option", { name: /Management Cluster \(local\)/i })
    ).toBeNull()
    expect(screen.getByTestId("cluster-placement-hint")).toHaveTextContent(
      /cluster-scoped resources/i
    )
    const clusterSelect = screen.getByLabelText(/Target Cluster/i)
    expect(clusterSelect).toHaveValue("")
    expect(clusterSelect).toHaveAttribute("aria-invalid", "true")
    expect(clusterSelect).toHaveAttribute(
      "aria-describedby",
      "cluster-placement-hint"
    )

    submit()
    expect(
      await screen.findByText(
        /can only be deployed to a remote workload cluster/i
      )
    ).toBeInTheDocument()
    expect(api.createProposal).not.toHaveBeenCalled()

    fireEvent.change(clusterSelect, { target: { value: "__custom__" } })
    fireEvent.change(screen.getByLabelText(/Custom kubeconfig Secret name/i), {
      target: { value: "local" },
    })
    submit()
    await waitFor(() =>
      expect(
        screen.getByText(/can only be deployed to a remote workload cluster/i)
      ).toBeInTheDocument()
    )
    expect(api.createProposal).not.toHaveBeenCalled()

    fireEvent.change(screen.getByLabelText(/Custom kubeconfig Secret name/i), {
      target: { value: "edge-kubeconfig" },
    })
    submit()
    await waitFor(() =>
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({
          catalogId: "cert-manager",
          targetCluster: "edge-kubeconfig",
        })
      )
    )
  })

  it("pins the target namespace of apps with a dedicated placement namespace", async () => {
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={{
          ...clusterScopedItem,
          id: "flux2",
          name: "Flux CD",
          placementNamespace: "flux-system",
        }}
        tenant="acme"
        stacks={[kindStack]}
      />
    )
    const namespaceInput = screen.getByLabelText(/Target Namespace/i)
    expect(namespaceInput).toHaveValue("flux-system")
    expect(namespaceInput).toBeDisabled()
    expect(namespaceInput).toHaveAttribute(
      "title",
      expect.stringMatching(/dedicated flux-system namespace/)
    )
    submit()
    await waitFor(() =>
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({
          catalogId: "flux2",
          targetCluster: "dev-kind-kubeconfig",
          targetNamespace: "flux-system",
        })
      )
    )
  })

  it("preselects the first workload cluster for cluster-scoped apps", async () => {
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={clusterScopedItem}
        tenant="acme"
        stacks={[kindStack]}
      />
    )
    expect(screen.getByLabelText(/Target Cluster/i)).toHaveValue(
      "dev-kind-kubeconfig"
    )
    submit()
    await waitFor(() =>
      expect(api.createProposal).toHaveBeenCalledWith(
        expect.objectContaining({ targetCluster: "dev-kind-kubeconfig" })
      )
    )
  })

  it("only offers and preselects workload clusters of the target tenant", () => {
    const betaStack: StackSummary = {
      ...kindStack,
      id: "beta-kind",
      name: "beta-kind",
      namespace: "tenant-beta",
      tenant: "beta",
    }
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={clusterScopedItem}
        tenant="acme"
        tenants={[
          { id: "acme", namespace: "tenant-acme" },
          { id: "beta", namespace: "tenant-beta" },
        ]}
        stacks={[betaStack, kindStack]}
      />
    )
    const clusterSelect = screen.getByLabelText(/Target Cluster/i)
    // beta's stack is listed first but belongs to another tenant: never preselected or offered.
    expect(clusterSelect).toHaveValue("dev-kind-kubeconfig")
    expect(screen.queryByRole("option", { name: /beta-kind/i })).toBeNull()

    fireEvent.change(screen.getByLabelText(/Target Tenant/i), {
      target: { value: "beta" },
    })
    // Switching tenant drops acme's cluster; the user picks one of beta's explicitly.
    expect(clusterSelect).toHaveValue("")
    expect(
      screen.getByRole("option", { name: /beta-kind/i })
    ).toBeInTheDocument()
    expect(screen.queryByRole("option", { name: /dev-kind/i })).toBeNull()
  })

  it("never preselects a cluster when editing a release without a recorded target", async () => {
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={clusterScopedItem}
        tenant="acme"
        stacks={[kindStack]}
        isEditMode
        initialConfig={{
          name: "cert-manager",
          tenant: "acme",
          values: {},
          targetCluster: "",
        }}
      />
    )
    expect(screen.getByLabelText(/Target Cluster/i)).toHaveValue("")
    submit()
    expect(
      await screen.findByText(
        /can only be deployed to a remote workload cluster/i
      )
    ).toBeInTheDocument()
    expect(api.createProposal).not.toHaveBeenCalled()
  })

  it("keeps the management cluster default for namespaced apps", () => {
    render(
      <DeployInstanceModal
        isOpen
        onClose={vi.fn()}
        catalogItem={sampleAppItem}
        tenant="acme"
        stacks={[kindStack]}
      />
    )
    expect(
      screen.getByRole("option", { name: /Management Cluster \(local\)/i })
    ).toBeInTheDocument()
    expect(screen.getByLabelText(/Target Cluster/i)).toHaveValue("")
    expect(screen.queryByTestId("cluster-placement-hint")).toBeNull()
  })
})
