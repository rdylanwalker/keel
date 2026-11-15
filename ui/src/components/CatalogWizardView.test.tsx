import {
  render,
  screen,
  fireEvent,
  waitFor,
  within,
} from "@testing-library/react"
import { describe, it, expect, vi, beforeEach } from "vitest"
import { CatalogWizardView } from "./CatalogWizardView"
import * as api from "../lib/api"
import { CatalogItem } from "../types/api"
import { jsonToYaml } from "../lib/utils"

vi.mock("../lib/api", () => ({
  getCatalog: vi.fn(),
  fetchCatalogSchema: vi.fn(),
  getStacks: vi.fn().mockResolvedValue([]),
  getReleases: vi.fn().mockResolvedValue([]),
  getReleaseConfig: vi.fn(),
  createProposal: vi.fn(),
  getTenants: vi
    .fn()
    .mockResolvedValue([{ id: "default", namespace: "tenant-default" }]),
  createTenant: vi.fn(),
  deleteTenant: vi.fn(),
  createCatalogApp: vi.fn(),
  updateCatalogItem: vi.fn(),
}))

const mockCatalog: CatalogItem[] = [
  {
    id: "app-test-service",
    name: "Test Microservice",
    description: "Mock test application catalog blueprint",
    type: "app",
    category: "Testing",
    version: "1.0.0",
    schema: {
      type: "object",
      properties: {
        serviceName: { type: "string", default: "test-service" },
      },
    },
    defaultValues: { serviceName: "test-service" },
  },
]

describe("CatalogWizardView component and jsonToYaml helper", () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it("tests jsonToYaml formatting helper for all primitive and object types", () => {
    expect(jsonToYaml(null)).toBe("null\n")
    expect(jsonToYaml(undefined)).toBe("null\n")
    expect(jsonToYaml(true)).toBe("true\n")
    expect(jsonToYaml(42)).toBe("42\n")
    expect(jsonToYaml("simple")).toBe("simple\n")
    expect(jsonToYaml("string: with colon")).toBe('"string: with colon"\n')
    expect(jsonToYaml(["a", "b"])).toBe("\n- a\n- b\n")
    expect(jsonToYaml([])).toBe("[]\n")
    expect(jsonToYaml({})).toBe("{}\n")
    expect(jsonToYaml({ name: "app", nested: { count: 3 } })).toContain(
      "name: app"
    )
  })

  it("renders catalog wizard and loads catalog items from API", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)

    render(<CatalogWizardView />)

    expect(
      screen.getByText("Self-Service Catalog & Dynamic Form Wizard")
    ).toBeInTheDocument()

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })
  })

  it("filters catalog items via search input", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    const searchInput = screen.getByPlaceholderText(
      /Search catalog blueprints/i
    )
    fireEvent.change(searchInput, {
      target: { value: "non-existent-blueprint" },
    })

    const list = screen.getByTestId("catalog-list")
    expect(within(list).queryByText("Test Microservice")).toBeNull()

    fireEvent.change(searchInput, { target: { value: "Microservice" } })
    expect(within(list).getByText("Test Microservice")).toBeInTheDocument()

    fireEvent.change(searchInput, { target: { value: "blueprint" } })
    expect(within(list).getByText("Test Microservice")).toBeInTheDocument()

    fireEvent.change(searchInput, { target: { value: "Testing" } })
    expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
  })

  it("submits proposal successfully and renders notification", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createProposal).mockResolvedValue({
      id: "99",
      title: "Deploy Test Microservice for default",
      branch: "keel/default/test-99",
      author: "user",
      tenant: "default",
      status: "open",
      createdAt: "just now",
      prUrl: "https://git.internal/pr/99",
    })

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    const submitBtn = screen.getByRole("button", {
      name: /Submit & Open Git PR Proposal/i,
    })
    fireEvent.click(submitBtn)

    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith({
        catalogId: "app-test-service",
        tenant: "default",
        values: { serviceName: "test-service" },
        title: "Deploy Test Microservice for default",
        kind: "app",
      })
      expect(
        screen.getByText("Git PR Proposal Successfully Opened!")
      ).toBeInTheDocument()
      expect(screen.getByText("View PR on Forge")).toBeInTheDocument()
    })
  })

  it("handles error on proposal submission failure", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createProposal).mockRejectedValue(
      new Error("SOPS encryption key missing")
    )

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    const submitBtn = screen.getByRole("button", {
      name: /Submit & Open Git PR Proposal/i,
    })
    fireEvent.click(submitBtn)

    expect(
      await screen.findByText("SOPS encryption key missing")
    ).toBeInTheDocument()
  })

  it("renders clean empty state on empty catalog response in non-demo mode", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([])

    render(<CatalogWizardView />)

    await waitFor(() => {
      expect(
        screen.getByText(
          "No catalog blueprints available — check server configuration or authenticate."
        )
      ).toBeInTheDocument()
    })
  })

  it("initializes with DEFAULT_CATALOG when demo=true is in location search", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getCatalog).mockResolvedValue([])

    render(<CatalogWizardView />)

    await waitFor(() => {
      expect(
        screen.getAllByText("Web Service Application").length
      ).toBeGreaterThan(0)
      expect(screen.getByText("proxmox")).toBeInTheDocument()
    })
    window.history.replaceState({}, "", "/")
  })

  it("handles keyboard navigation and tenant selection", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "default", namespace: "tenant-default" },
      { id: "acme", namespace: "tenant-acme" },
    ])

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    const tenantSelect = screen.getByLabelText(/Target Tenant:/i)
    fireEvent.change(tenantSelect, { target: { value: "acme" } })

    const list = screen.getByTestId("catalog-list")
    const itemCard = within(list).getByRole("button")
    fireEvent.keyDown(itemCard, { key: "Enter" })
    fireEvent.keyDown(itemCard, { key: " " })
  })

  it("handles catalog API rejection gracefully and proposal string error", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createProposal).mockRejectedValue("Proposal timeout")

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    const submitBtn = screen.getByRole("button", {
      name: /Submit & Open Git PR Proposal/i,
    })
    fireEvent.click(submitBtn)

    await waitFor(() => {
      expect(screen.getByRole("alert")).toHaveTextContent(
        "Failed to create Git PR proposal"
      )
    })
  })

  it("creates a new tenant via modal form", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "default", namespace: "tenant-default" },
    ])
    vi.mocked(api.createTenant).mockResolvedValue({
      id: "105",
      title: "Create Tenant acme",
      branch: "keel/tenant-acme/105",
      author: "user",
      tenant: "acme",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const newTenantBtn = screen.getByRole("button", { name: /\+ New Tenant/i })
    fireEvent.click(newTenantBtn)

    expect(screen.getByText("Create New Tenant")).toBeInTheDocument()

    const tenantInput = screen.getByLabelText(/Tenant ID/i)
    fireEvent.change(tenantInput, { target: { value: "acme" } })

    const submitTenantBtn = screen.getByRole("button", {
      name: /Create Tenant PR/i,
    })
    fireEvent.click(submitTenantBtn)

    await waitFor(() => {
      expect(api.createTenant).toHaveBeenCalledWith("acme")
      expect(
        screen.getByText("Git PR Proposal Successfully Opened!")
      ).toBeInTheDocument()
    })
  })

  it("removes non-default tenant", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "default", namespace: "tenant-default" },
      { id: "globex", namespace: "tenant-globex" },
    ])
    vi.mocked(api.deleteTenant).mockResolvedValue({
      id: "106",
      title: "Delete Tenant globex",
      branch: "keel/tenant-globex/106",
      author: "user",
      tenant: "globex",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const select = await screen.findByLabelText(/Target Tenant:/i)
    await waitFor(() => {
      expect(
        within(select).getByRole("option", { name: "globex" })
      ).toBeInTheDocument()
    })
    fireEvent.change(select, { target: { value: "globex" } })

    const removeBtn = await screen.findByRole("button", { name: /Remove/i })
    fireEvent.click(removeBtn)

    await waitFor(() => {
      expect(api.deleteTenant).toHaveBeenCalledWith("globex")
      expect(
        screen.getByText("Git PR Proposal Successfully Opened!")
      ).toBeInTheDocument()
    })
  })

  it("adds catalog app via modal form", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createCatalogApp).mockResolvedValue({
      id: "107",
      title: "Add Catalog App podinfo",
      branch: "keel/catalog/107",
      author: "user",
      tenant: "default",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const addAppBtn = screen.getByRole("button", {
      name: /\+ Add Catalog Item/i,
    })
    fireEvent.click(addAppBtn)

    expect(screen.getByText("Add Catalog Item")).toBeInTheDocument()

    fireEvent.change(screen.getByLabelText(/(App|Item) ID \*/i), {
      target: { value: "podinfo" },
    })
    fireEvent.change(screen.getByLabelText(/(App|Item) Name \*/i), {
      target: { value: "Podinfo" },
    })
    fireEvent.change(screen.getByLabelText(/Category \*/i), {
      target: { value: "Applications" },
    })
    fireEvent.change(screen.getByLabelText(/Repository URL \*/i), {
      target: { value: "https://github.com/stefanprodan/podinfo" },
    })
    fireEvent.change(screen.getByLabelText(/Chart Name \(Helm\) \/ Path/i), {
      target: { value: "podinfo" },
    })
    fireEvent.change(screen.getByLabelText(/Version/i), {
      target: { value: "6.7.0" },
    })

    const submitAppBtn = screen.getByRole("button", {
      name: /Submit App PR Proposal/i,
    })
    fireEvent.click(submitAppBtn)

    await waitFor(() => {
      expect(api.createCatalogApp).toHaveBeenCalledWith({
        kind: "app",
        id: "podinfo",
        name: "Podinfo",
        category: "Applications",
        runtime: "helm",
        repoUrl: "https://github.com/stefanprodan/podinfo",
        chart: "podinfo",
        version: "6.7.0",
        flaggerSupported: false,
      })
      expect(
        screen.getByText("Git PR Proposal Successfully Opened!")
      ).toBeInTheDocument()
    })
  })

  it("adds catalog infra blueprint via modal form when switching type to infra", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createCatalogApp).mockResolvedValue({
      id: "108",
      title: "Add Catalog Item proxmox-k8s",
      branch: "keel/catalog/108",
      author: "user",
      tenant: "default",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const addItemBtn = screen.getByRole("button", {
      name: /\+ Add Catalog Item/i,
    })
    fireEvent.click(addItemBtn)

    expect(screen.getByText("Add Catalog Item")).toBeInTheDocument()

    // Test switching to infra then back to app to test state transitions
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "infra" },
    })
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "app" },
    })

    // Customize app fields to hit all onChange handlers
    fireEvent.change(screen.getByLabelText(/Runtime \*/i), {
      target: { value: "kustomize" },
    })
    fireEvent.change(screen.getByLabelText(/Chart Name \(Helm\) \/ Path/i), {
      target: { value: "./base" },
    })
    fireEvent.change(screen.getByLabelText(/Version/i), {
      target: { value: "1.2.3" },
    })
    fireEvent.click(screen.getByLabelText(/Flagger Canary Support Enabled/i))

    // Switch type back to infra
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "infra" },
    })

    fireEvent.change(screen.getByLabelText(/(App|Item) ID \*/i), {
      target: { value: "proxmox-k8s" },
    })
    fireEvent.change(screen.getByLabelText(/(App|Item) Name \*/i), {
      target: { value: "Proxmox K8s Cluster" },
    })
    fireEvent.change(screen.getByLabelText(/Category \*/i), {
      target: { value: "Cloud Stacks" },
    })
    fireEvent.change(screen.getByLabelText(/^Provider \*/i), {
      target: { value: "proxmox" },
    })
    fireEvent.change(screen.getByLabelText(/Entrypoint/i), {
      target: { value: "main.yaml" },
    })
    fireEvent.change(screen.getByLabelText(/Pulumi Runtime \*/i), {
      target: { value: "opentofu-bridge" },
    })
    fireEvent.change(screen.getByLabelText(/OpenTofu \/ Terraform Provider/i), {
      target: { value: "bpg/proxmox" },
    })
    fireEvent.click(screen.getByLabelText(/Protect by Default/i))

    const submitInfraBtn = screen.getByRole("button", {
      name: /Submit Infra PR Proposal/i,
    })
    fireEvent.click(submitInfraBtn)

    await waitFor(() => {
      expect(api.createCatalogApp).toHaveBeenCalledWith({
        kind: "infra",
        id: "proxmox-k8s",
        name: "Proxmox K8s Cluster",
        category: "Cloud Stacks",
        provider: "proxmox",
        pulumiRuntime: "opentofu-bridge",
        entrypoint: "main.yaml",
        terraformProvider: "bpg/proxmox",
        protectByDefault: true,
      })
      expect(
        screen.getByText("Git PR Proposal Successfully Opened!")
      ).toBeInTheDocument()
    })
  })

  it("submits infra blueprint with default entrypoint and omitted terraform provider", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createCatalogApp).mockResolvedValue({
      id: "109",
      title: "Add Catalog Item kind-local",
      branch: "keel/catalog/109",
      author: "user",
      tenant: "default",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const addItemBtn = screen.getByRole("button", {
      name: /\+ Add Catalog Item/i,
    })
    fireEvent.click(addItemBtn)

    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "infra" },
    })

    fireEvent.change(screen.getByLabelText(/Item ID \*/i), {
      target: { value: "kind-local" },
    })
    fireEvent.change(screen.getByLabelText(/Item Name \*/i), {
      target: { value: "Kind Local Cluster" },
    })

    const submitInfraBtn = screen.getByRole("button", {
      name: /Submit Infra PR Proposal/i,
    })
    fireEvent.click(submitInfraBtn)

    await waitFor(() => {
      expect(api.createCatalogApp).toHaveBeenCalledWith({
        kind: "infra",
        id: "kind-local",
        name: "Kind Local Cluster",
        category: "Infrastructure",
        provider: "kind",
        pulumiRuntime: "yaml",
        entrypoint: "Pulumi.yaml",
        terraformProvider: undefined,
        protectByDefault: false,
      })
    })
  })

  it("handles tenant creation and deletion errors and cancel button", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.getTenants).mockResolvedValue([
      { id: "default", namespace: "tenant-default" },
      { id: "stark", namespace: "tenant-stark" },
    ])
    vi.mocked(api.createTenant).mockRejectedValue(
      new Error("Tenant quota exceeded")
    )
    vi.mocked(api.deleteTenant).mockRejectedValue("String delete error")

    render(<CatalogWizardView />)

    // Open and cancel new tenant modal
    const newTenantBtn = screen.getByRole("button", { name: /\+ New Tenant/i })
    fireEvent.click(newTenantBtn)
    expect(screen.getByText("Create New Tenant")).toBeInTheDocument()

    const cancelTenantBtn = screen.getByRole("button", { name: /Cancel/i })
    fireEvent.click(cancelTenantBtn)
    expect(screen.queryByText("Create New Tenant")).not.toBeInTheDocument()

    // Open and fail creation
    fireEvent.click(newTenantBtn)
    fireEvent.change(screen.getByLabelText(/Tenant ID/i), {
      target: { value: "error-tenant" },
    })
    fireEvent.click(screen.getByRole("button", { name: /Create Tenant PR/i }))

    expect(await screen.findByText("Tenant quota exceeded")).toBeInTheDocument()

    // Remove tenant string error failure
    const select = await screen.findByLabelText(/Target Tenant:/i)
    fireEvent.change(select, { target: { value: "stark" } })

    const removeBtn = await screen.findByRole("button", { name: /Remove/i })
    fireEvent.click(removeBtn)

    expect(
      await screen.findByText("Failed to remove tenant proposal")
    ).toBeInTheDocument()
  })

  it("handles catalog app creation errors (string error) and cancel button", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createCatalogApp).mockRejectedValue("String error rejection")

    render(<CatalogWizardView />)

    const addAppBtn = screen.getByRole("button", {
      name: /\+ Add Catalog Item/i,
    })
    fireEvent.click(addAppBtn)
    expect(screen.getByText("Add Catalog Item")).toBeInTheDocument()

    // Change category in app mode
    fireEvent.change(screen.getByLabelText(/Category \*/i), {
      target: { value: "Custom App Category" },
    })

    // Cancel modal via close icon
    const closeBtn = screen.getByRole("button", { name: /Close modal/i })
    fireEvent.click(closeBtn)
    expect(screen.queryByText("Add Catalog Item")).not.toBeInTheDocument()

    // Fail app creation with string error
    fireEvent.click(addAppBtn)
    fireEvent.change(screen.getByLabelText(/(App|Item) ID \*/i), {
      target: { value: "fail-app" },
    })
    fireEvent.change(screen.getByLabelText(/(App|Item) Name \*/i), {
      target: { value: "Fail App" },
    })
    fireEvent.change(screen.getByLabelText(/Repository URL \*/i), {
      target: { value: "https://git.test/app" },
    })
    fireEvent.click(
      screen.getByRole("button", { name: /Submit App PR Proposal/i })
    )

    expect(
      await screen.findByText("Failed to create catalog app proposal")
    ).toBeInTheDocument()
  })

  it("preserves schema default values in Live values.yaml Preview when clicking catalog items", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "kind-cluster",
        name: "Local kind Cluster",
        description: "Provision kind",
        type: "infra",
        category: "Infrastructure",
        provider: "kind",
        version: "1.0.0",
        schema: {
          type: "object",
          properties: {
            clusterName: { type: "string", default: "workload-dev" },
            workerCount: { type: "integer", default: 1 },
          },
        },
      },
      {
        id: "podinfo",
        name: "Podinfo",
        description: "Podinfo app",
        type: "app",
        category: "Apps",
        version: "6.7.1",
        schema: {
          type: "object",
          properties: {
            replicaCount: { type: "integer", default: 2 },
          },
        },
      },
    ])

    render(<CatalogWizardView />)

    expect(
      await screen.findByText(/clusterName: workload-dev/)
    ).toBeInTheDocument()

    // Click second item, then click first item again
    fireEvent.click(screen.getByText("Podinfo"))
    expect(await screen.findByText(/replicaCount: 2/)).toBeInTheDocument()

    fireEvent.click(screen.getByText("Local kind Cluster"))
    expect(
      await screen.findByText(/clusterName: workload-dev/)
    ).toBeInTheDocument()
    expect(screen.queryByText("{}")).not.toBeInTheDocument()
  })

  it("handles catalog API rejection in demo mode gracefully", async () => {
    window.history.replaceState({}, "", "/?demo=true")
    vi.mocked(api.getCatalog).mockRejectedValue(new Error("Network error"))

    render(<CatalogWizardView />)

    await waitFor(() => {
      expect(screen.getByText("Network error")).toBeInTheDocument()
    })
    window.history.replaceState({}, "", "/")
  })

  it("submits infrastructure proposal with kind='infra'", async () => {
    const mockInfraCatalog: CatalogItem[] = [
      {
        id: "infra-pko-vm",
        name: "Proxmox VM Stack",
        description: "PKO infrastructure blueprint",
        type: "infrastructure",
        category: "Infrastructure",
        version: "1.0.0",
        schema: { type: "object", properties: {} },
        defaultValues: {},
      },
    ]
    vi.mocked(api.getCatalog).mockResolvedValue(mockInfraCatalog)
    vi.mocked(api.createProposal).mockResolvedValue({
      id: "108",
      title: "Deploy Proxmox VM Stack for default",
      branch: "keel/default/infra-108",
      author: "user",
      tenant: "default",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Proxmox VM Stack")).toBeInTheDocument()
    })

    const submitBtn = screen.getByRole("button", {
      name: /Submit & Open Git PR Proposal/i,
    })
    fireEvent.click(submitBtn)

    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith({
        catalogId: "infra-pko-vm",
        tenant: "default",
        values: {},
        title: "Deploy Proxmox VM Stack for default",
        kind: "infra",
      })
    })
  })

  it("submits custom instance name, chart version, and target namespace for remote cluster", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "podinfo",
        name: "Podinfo",
        description: "Podinfo app",
        type: "app",
        category: "Apps",
        version: "6.7.1",
        schema: {
          type: "object",
          properties: {
            replicaCount: { type: "integer", default: 1 },
          },
        },
      },
    ])
    vi.mocked(api.getStacks).mockResolvedValue([
      {
        id: "stack-1",
        name: "workload-eu",
        tenant: "default",
        type: "workload",
        provider: "kind",
        status: "synced",
        resourceCount: 5,
        lastSynced: "just now",
      },
    ])
    vi.mocked(api.createProposal).mockResolvedValue({
      id: "109",
      title: "Deploy podinfo-staging for default",
      branch: "keel/default/app-podinfo-109",
      author: "user",
      tenant: "default",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const instanceInput = await screen.findByLabelText(/Instance Name/i)
    fireEvent.change(instanceInput, { target: { value: "podinfo-staging" } })

    const versionInput = screen.getByLabelText(/Chart Version/i)
    fireEvent.change(versionInput, { target: { value: "6.6.0" } })

    // Target Namespace is disabled when local cluster is selected
    const nsInput = screen.getByLabelText(/Target Namespace/i)
    expect(nsInput).toBeDisabled()

    // Select remote workload cluster
    const clusterSelect = screen.getByLabelText(/Target Cluster/i)
    fireEvent.change(clusterSelect, {
      target: { value: "workload-eu-kubeconfig" },
    })

    expect(nsInput).not.toBeDisabled()
    fireEvent.change(nsInput, { target: { value: "staging-apps" } })

    fireEvent.click(
      screen.getByRole("button", { name: /Submit & Open Git PR Proposal/i })
    )

    await waitFor(() => {
      expect(api.createProposal).toHaveBeenCalledWith({
        catalogId: "podinfo",
        tenant: "default",
        name: "podinfo-staging",
        version: "6.6.0",
        targetCluster: "workload-eu-kubeconfig",
        targetNamespace: "staging-apps",
        values: { replicaCount: 1 },
        title: "Deploy podinfo-staging for default",
        kind: "app",
      })
    })
  })

  it("shows a non-blocking warning when Instance Name matches an existing release in the tenant", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "podinfo",
        name: "Podinfo",
        description: "Podinfo app",
        type: "app",
        category: "Apps",
        version: "6.7.1",
        schema: { type: "object", properties: {} },
      },
    ])
    vi.mocked(api.getReleases).mockResolvedValue([
      {
        id: "rel-1",
        name: "podinfo",
        tenant: "default",
        namespace: "tenant-default",
        type: "helm",
        status: "healthy",
        version: "6.7.1",
      },
    ])

    render(<CatalogWizardView />)

    expect(await screen.findByText(/already exists in/i)).toBeInTheDocument()

    // Changing Instance Name to a new name clears the warning
    const instanceInput = screen.getByLabelText(/Instance Name/i)
    fireEvent.change(instanceInput, { target: { value: "podinfo-v2" } })
    expect(screen.queryByText(/already exists in/i)).not.toBeInTheDocument()
  })

  it("pre-fills RJSF form and metadata when initialReleaseTarget is provided", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "podinfo",
        name: "Podinfo",
        description: "Podinfo app",
        type: "app",
        category: "Apps",
        version: "6.7.1",
        schema: {
          type: "object",
          properties: {
            replicaCount: { type: "integer", default: 1 },
          },
        },
      },
    ])
    vi.mocked(api.getReleaseConfig).mockResolvedValue({
      catalogId: "podinfo",
      tenant: "default",
      name: "podinfo-prod",
      version: "6.6.0",
      values: { replicaCount: 4 },
    })
    const onClear = vi.fn()

    render(
      <CatalogWizardView
        initialReleaseTarget={{
          namespace: "tenant-default",
          name: "podinfo-prod",
        }}
        onClearReleaseTarget={onClear}
      />
    )

    await waitFor(() => {
      expect(api.getReleaseConfig).toHaveBeenCalledWith(
        "tenant-default",
        "podinfo-prod"
      )
    })

    expect(await screen.findByDisplayValue("podinfo-prod")).toBeInTheDocument()
    expect(screen.getByDisplayValue("6.6.0")).toBeInTheDocument()
    expect(screen.getByText(/replicaCount: 4/)).toBeInTheDocument()
    expect(onClear).toHaveBeenCalled()
  })

  it("edits an existing catalog item via modal and submits PUT update PR", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "podinfo",
        name: "Podinfo",
        description: "Podinfo app",
        type: "app",
        category: "Apps",
        version: "6.7.1",
        runtime: "helm",
        repoUrl: "https://github.com/stefanprodan/podinfo",
        chart: "podinfo",
        flaggerSupported: false,
        schema: {
          type: "object",
          properties: {
            replicaCount: { type: "integer", default: 1 },
          },
        },
      },
    ])
    vi.mocked(api.updateCatalogItem).mockResolvedValue({
      id: "112",
      title: "feat(catalog): update app podinfo",
      branch: "keel/catalog/update-podinfo-112",
      author: "user",
      tenant: "catalog",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const editBtn = await screen.findByRole("button", { name: /^Edit$/i })
    fireEvent.click(editBtn)

    expect(screen.getByText("Edit Catalog Item")).toBeInTheDocument()
    expect(screen.getByLabelText(/Catalog Type \*/i)).toBeDisabled()
    expect(screen.getByLabelText(/Item ID \*/i)).toBeDisabled()

    fireEvent.change(screen.getByLabelText(/Item Name \*/i), {
      target: { value: "Podinfo Pro" },
    })
    fireEvent.change(screen.getByLabelText(/^Version$/i), {
      target: { value: "6.8.0" },
    })

    fireEvent.click(screen.getByRole("button", { name: /Submit Update PR/i }))

    await waitFor(() => {
      expect(api.updateCatalogItem).toHaveBeenCalledWith("app", "podinfo", {
        name: "Podinfo Pro",
        category: "Apps",
        description: "Podinfo app",
        runtime: "helm",
        repoUrl: "https://github.com/stefanprodan/podinfo",
        chart: "podinfo",
        version: "6.8.0",
        path: "",
        flaggerSupported: false,
        provider: undefined,
        pulumiRuntime: undefined,
        entrypoint: undefined,
        protectByDefault: undefined,
        items: undefined,
        schema: undefined,
      })
    })
    expect(await screen.findByText(/PR #112 Opened/i)).toBeInTheDocument()
  })
})
