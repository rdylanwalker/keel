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

    fireEvent.click(screen.getByRole("button", { name: /^Create$/i }))

    const submitBtn = await screen.findByRole("button", {
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

    fireEvent.click(screen.getByRole("button", { name: /^Create$/i }))

    const submitBtn = await screen.findByRole("button", {
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

  it("renders informational cards with Create and Edit buttons and supports tenant selection", async () => {
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

    // Clicking the card title/body does not open the deploy modal
    fireEvent.click(screen.getByText("Test Microservice"))
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument()

    // Clicking Create opens the deploy modal
    const list = screen.getByTestId("catalog-list")
    fireEvent.click(within(list).getByRole("button", { name: /^Create$/i }))
    expect(await screen.findByRole("dialog")).toBeInTheDocument()
  })

  it("handles catalog API rejection gracefully and proposal string error", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    vi.mocked(api.createProposal).mockRejectedValue("Proposal timeout")

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^Create$/i }))

    const submitBtn = await screen.findByRole("button", {
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

    const newTenantBtn = screen.getByRole("button", { name: /New Tenant/i })
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

    // Remove button stays mounted when default tenant is selected, but is disabled
    const disabledRemoveBtn = screen.getByRole("button", { name: /Remove/i })
    expect(disabledRemoveBtn).toBeDisabled()

    fireEvent.change(select, { target: { value: "globex" } })

    const removeBtn = await screen.findByRole("button", { name: /Remove/i })
    expect(removeBtn).toBeEnabled()
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
      name: /^Add Catalog Item$/i,
    })
    expect(addAppBtn).toHaveTextContent("Add Catalog Item")
    expect(addAppBtn).not.toHaveTextContent("+ Add Catalog Item")
    fireEvent.click(addAppBtn)

    expect(
      screen.getByRole("heading", { name: "Add Catalog Item" })
    ).toBeInTheDocument()

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
      name: /^Add Catalog Item$/i,
    })
    fireEvent.click(addItemBtn)

    expect(
      screen.getByRole("heading", { name: "Add Catalog Item" })
    ).toBeInTheDocument()

    // Test infra blueprint submission with default entrypoint and omitted terraform provider
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
      name: /^Add Catalog Item$/i,
    })
    fireEvent.click(addItemBtn)

    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "infra" },
    })
    fireEvent.change(screen.getByLabelText(/(App|Item) ID \*/i), {
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
    const newTenantBtn = screen.getByRole("button", { name: /New Tenant/i })
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
      name: /^Add Catalog Item$/i,
    })
    fireEvent.click(addAppBtn)
    expect(
      screen.getByRole("heading", { name: "Add Catalog Item" })
    ).toBeInTheDocument()

    // Test cancel button
    // Change category in app mode
    fireEvent.change(screen.getByLabelText(/Category \*/i), {
      target: { value: "Custom App Category" },
    })

    // Cancel modal via close icon
    const closeBtn = screen.getByRole("button", { name: /Close modal/i })
    fireEvent.click(closeBtn)
    expect(
      screen.queryByRole("heading", { name: "Add Catalog Item" })
    ).not.toBeInTheDocument()

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

    const createButtons = await screen.findAllByRole("button", {
      name: /^Create$/i,
    })
    fireEvent.click(createButtons[0])

    expect(
      await screen.findByText(/clusterName: workload-dev/)
    ).toBeInTheDocument()

    // Close modal and open second item's Create modal
    fireEvent.click(screen.getByRole("button", { name: /Close modal/i }))
    fireEvent.click(createButtons[1])
    expect(await screen.findByText(/replicaCount: 2/)).toBeInTheDocument()

    // Close modal and open first item's Create modal again
    fireEvent.click(screen.getByRole("button", { name: /Close modal/i }))
    fireEvent.click(createButtons[0])
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

    fireEvent.click(screen.getByRole("button", { name: /^Create$/i }))

    const submitBtn = await screen.findByRole("button", {
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

    const createBtn = await screen.findByRole("button", { name: /^Create$/i })
    fireEvent.click(createBtn)

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

    const createBtn = await screen.findByRole("button", { name: /^Create$/i })
    fireEvent.click(createBtn)

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

  it("supports adding and editing Pulumi.yaml on infrastructure blueprints", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "infra-pko-vm",
        name: "Proxmox VM Stack",
        description: "Provision a VM",
        type: "infra",
        category: "Infrastructure",
        version: "1.0.0",
        provider: "proxmox",
        pulumiRuntime: "yaml",
        entrypoint: "Pulumi.yaml",
        pulumiYaml: "name: proxmox-vm\nruntime: yaml\nresources: {}\n",
        schema: { type: "object", properties: {} },
      },
    ])
    vi.mocked(api.updateCatalogItem).mockResolvedValue({
      id: "115",
      title: "Update infra infra-pko-vm",
      branch: "keel/catalog/update-infra-pko-vm-115",
      author: "user",
      tenant: "catalog",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    const editBtn = await screen.findByRole("button", {
      name: /^Edit$/i,
    })
    fireEvent.click(editBtn)

    const pulumiInput = screen.getByLabelText(/Pulumi\.yaml Program/i)
    expect(pulumiInput).toHaveValue(
      "name: proxmox-vm\nruntime: yaml\nresources: {}\n"
    )

    fireEvent.change(pulumiInput, {
      target: {
        value:
          "name: proxmox-vm\nruntime: yaml\ndescription: Edited\nresources: {}\n",
      },
    })

    fireEvent.click(screen.getByRole("button", { name: /Submit Update PR/i }))

    await waitFor(() => {
      expect(api.updateCatalogItem).toHaveBeenCalledWith(
        "infra",
        "infra-pko-vm",
        expect.objectContaining({
          pulumiYaml:
            "name: proxmox-vm\nruntime: yaml\ndescription: Edited\nresources: {}\n",
        })
      )
    })
  })

  it("toggles full-screen side-by-side mode on Deploy, Add/Edit Blueprint, and New Tenant modals", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)

    render(<CatalogWizardView />)

    // 1. Deploy modal full-screen toggle
    const createBtn = await screen.findByRole("button", { name: /^Create$/i })
    fireEvent.click(createBtn)

    const deployModal = await screen.findByTestId("deploy-instance-modal")
    expect(deployModal.className).toContain("max-w-4xl")

    fireEvent.click(screen.getByRole("button", { name: /Enter full screen/i }))
    expect(deployModal.className).toContain("w-full h-full")
    expect(deployModal.className).toContain("rounded-none")

    fireEvent.click(screen.getByRole("button", { name: /Exit full screen/i }))
    expect(deployModal.className).toContain("max-w-4xl")
    fireEvent.click(screen.getByRole("button", { name: /Close modal/i }))

    // 2. Add/Edit Catalog Item modal full-screen toggle
    fireEvent.click(screen.getByRole("button", { name: /^Add Catalog Item$/i }))
    const catalogItemModal = screen.getByTestId("catalog-item-modal")
    expect(catalogItemModal.className).toContain("max-w-4xl")
    expect(catalogItemModal.className).toContain("w-full")
    fireEvent.click(screen.getByRole("button", { name: /Enter full screen/i }))
    expect(catalogItemModal.className).toContain("w-full h-full")
    expect(catalogItemModal.className).toContain("rounded-none")
    fireEvent.click(screen.getByRole("button", { name: /Exit full screen/i }))
    expect(catalogItemModal.className).toContain("max-w-4xl")
    expect(
      screen.getByRole("button", { name: /Enter full screen/i })
    ).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: /Close modal/i }))

    // 3. New Tenant modal full-screen toggle
    fireEvent.click(screen.getByRole("button", { name: /New Tenant/i }))
    const tenantModal = screen.getByTestId("new-tenant-modal")
    expect(tenantModal.className).toContain("max-w-4xl")
    expect(tenantModal.className).toContain("w-full")
    fireEvent.click(screen.getByRole("button", { name: /Enter full screen/i }))
    expect(tenantModal.className).toContain("w-full h-full")
    expect(tenantModal.className).toContain("rounded-none")
    fireEvent.click(screen.getByRole("button", { name: /Exit full screen/i }))
    expect(tenantModal.className).toContain("max-w-4xl")
    expect(
      screen.getByRole("button", { name: /Enter full screen/i })
    ).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: /Close modal/i }))
  })

  it("derives JSON Schema fields from Pulumi.yaml and adds provider credentials", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^Add Catalog Item$/i }))
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "infra" },
    })
    fireEvent.change(screen.getByLabelText(/^Provider/i), {
      target: { value: "gcp" },
    })

    fireEvent.change(screen.getByLabelText(/Pulumi\.yaml Program/i), {
      target: {
        value:
          "name: gcp-free-vm\nruntime: yaml\nconfig:\n  project:\n    type: string\n",
      },
    })

    fireEvent.click(
      screen.getByRole("button", { name: /Derive from Pulumi\.yaml/i })
    )
    fireEvent.click(
      screen.getByRole("button", { name: /\+ Add gcp Credentials/i })
    )

    const schemaTextarea = screen.getByLabelText(
      /JSON Schema/i
    ) as HTMLTextAreaElement
    const parsed = JSON.parse(schemaTextarea.value)
    expect(parsed.required).toEqual(["project", "GOOGLE_CREDENTIALS"])
    expect(parsed.properties.GOOGLE_CREDENTIALS.writeOnly).toBe(true)
    expect(parsed.properties.GOOGLE_CREDENTIALS.format).toBe("password")
  })

  it("shows Enable Flagger Canary option for Flagger-supported apps in bundle builder and submits canary: true", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "kind-cluster",
        name: "kind Cluster",
        description: "Local cluster",
        type: "infra",
        category: "Infrastructure",
        version: "1.0.0",
        provider: "kind",
        schema: { type: "object", properties: {} },
      },
      {
        id: "podinfo",
        name: "Podinfo",
        description: "Canary-ready app",
        type: "app",
        category: "Applications",
        version: "6.7.1",
        flaggerSupported: true,
        schema: { type: "object", properties: {} },
      },
      {
        id: "redis",
        name: "Redis",
        description: "Non-canary app",
        type: "app",
        category: "Applications",
        version: "20.1.0",
        flaggerSupported: false,
        schema: { type: "object", properties: {} },
      },
    ])
    vi.mocked(api.createCatalogApp).mockResolvedValue({
      id: "201",
      title: "Add bundle canary-stack",
      branch: "keel/catalog/add-canary-stack-201",
      author: "user",
      tenant: "catalog",
      status: "open",
      createdAt: "just now",
    })

    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Podinfo")).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^Add Catalog Item$/i }))
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "bundle" },
    })

    fireEvent.change(screen.getByLabelText(/Item ID \*/i), {
      target: { value: "canary-stack" },
    })
    fireEvent.change(screen.getByLabelText(/Item Name \*/i), {
      target: { value: "Canary Stack" },
    })

    // Add an app step (defaults to first app, which is podinfo with flaggerSupported: true)
    fireEvent.click(screen.getByRole("button", { name: /Add Step/i }))

    const canaryCheckbox = screen.getByLabelText(
      /Step 2 Enable Flagger Canary/i
    )
    expect(canaryCheckbox).not.toBeChecked()
    fireEvent.click(canaryCheckbox)
    expect(canaryCheckbox).toBeChecked()

    // Switching to an app without flaggerSupported hides and clears the option
    fireEvent.change(screen.getByLabelText(/Step 2 Catalog Ref/i), {
      target: { value: "redis" },
    })
    expect(
      screen.queryByLabelText(/Step 2 Enable Flagger Canary/i)
    ).not.toBeInTheDocument()

    // Switch back to podinfo and re-enable canary
    fireEvent.change(screen.getByLabelText(/Step 2 Catalog Ref/i), {
      target: { value: "podinfo" },
    })
    const reappearedCheckbox = screen.getByLabelText(
      /Step 2 Enable Flagger Canary/i
    )
    expect(reappearedCheckbox).not.toBeChecked()
    fireEvent.click(reappearedCheckbox)

    // Configure Depends on, Target Cluster From, and Target Namespace on Step 2
    const depCheckbox = screen.getByRole("checkbox", { name: /step-1/i })
    expect(depCheckbox).toBeChecked()
    fireEvent.click(depCheckbox)
    expect(depCheckbox).not.toBeChecked()
    fireEvent.click(depCheckbox)
    expect(depCheckbox).toBeChecked()

    fireEvent.change(screen.getByLabelText(/Step 2 Target Cluster From/i), {
      target: { value: "step-1" },
    })
    fireEvent.change(screen.getByLabelText(/Step 2 Target Namespace/i), {
      target: { value: "canary-ns" },
    })

    fireEvent.click(
      screen.getByRole("button", { name: /Submit Bundle PR Proposal/i })
    )

    await waitFor(() => {
      expect(api.createCatalogApp).toHaveBeenCalledWith(
        expect.objectContaining({
          kind: "bundle",
          id: "canary-stack",
          items: expect.arrayContaining([
            expect.objectContaining({
              id: "step-2",
              kind: "app",
              catalogRef: "podinfo",
              canary: true,
            }),
          ]),
        })
      )
    })
  })

  it("supports interactive Schema Editor field management, raw JSON editing, and cancel", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^Add Catalog Item$/i }))

    // Expand the Schema Editor
    fireEvent.click(screen.getByRole("button", { name: /Edit JSON Schema/i }))

    // Add two schema fields
    const addFieldBtn = screen.getByRole("button", { name: /\+ Add Field/i })
    fireEvent.click(addFieldBtn)
    fireEvent.click(addFieldBtn)

    // Edit first field's name, title, type, default, required, and secret
    fireEvent.change(screen.getByLabelText("Field 1 name"), {
      target: { value: "replicas" },
    })
    fireEvent.change(screen.getByLabelText("Field 1 title"), {
      target: { value: "Replicas" },
    })
    fireEvent.change(screen.getByLabelText("Field 1 type"), {
      target: { value: "integer" },
    })
    fireEvent.change(screen.getByLabelText("Field 1 default"), {
      target: { value: "3" },
    })

    const reqCheckboxes = screen.getAllByTitle("Required")
    const secCheckboxes = screen.getAllByTitle(/Secret/i)
    fireEvent.click(within(reqCheckboxes[0]).getByRole("checkbox"))
    fireEvent.click(within(secCheckboxes[0]).getByRole("checkbox"))

    // Remove second field
    fireEvent.click(screen.getByRole("button", { name: "Remove field 2" }))

    const schemaTextarea = screen.getByLabelText(
      /JSON Schema/i
    ) as HTMLTextAreaElement
    const parsed = JSON.parse(schemaTextarea.value)
    expect(parsed.required).toContain("replicas")
    expect(parsed.properties.replicas.type).toBe("integer")
    expect(parsed.properties.replicas.writeOnly).toBe(true)

    // Edit raw JSON Schema directly
    fireEvent.change(schemaTextarea, {
      target: {
        value: JSON.stringify(
          {
            type: "object",
            properties: {
              domain: { type: "string", default: "example.com" },
            },
          },
          null,
          2
        ),
      },
    })
    expect(
      (screen.getByLabelText("Field 1 name") as HTMLInputElement).value
    ).toBe("domain")

    // Cancel closes the modal
    fireEvent.click(screen.getByRole("button", { name: /^Cancel$/i }))
    expect(screen.queryByTestId("catalog-item-modal")).not.toBeInTheDocument()
  })

  it("supports nested object fields and array/list fields in the JSON Schema Form Builder", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^Add Catalog Item$/i }))
    fireEvent.click(screen.getByRole("button", { name: /Edit JSON Schema/i }))

    const addFieldBtn = screen.getByRole("button", { name: /\+ Add Field/i })
    fireEvent.click(addFieldBtn)
    fireEvent.click(addFieldBtn)
    fireEvent.click(addFieldBtn)

    // Field 1: nested object "db" with child "host"
    fireEvent.change(screen.getByLabelText("Field 1 name"), {
      target: { value: "db" },
    })
    fireEvent.change(screen.getByLabelText("Field 1 type"), {
      target: { value: "object" },
    })
    fireEvent.click(
      screen.getByRole("button", { name: "Add nested field to 1" })
    )
    fireEvent.change(screen.getByLabelText("Field 1.1 name"), {
      target: { value: "host" },
    })
    fireEvent.change(screen.getByLabelText("Field 1.1 default"), {
      target: { value: "localhost" },
    })

    // Field 2: array of integers "ports"
    fireEvent.change(screen.getByLabelText("Field 2 name"), {
      target: { value: "ports" },
    })
    fireEvent.change(screen.getByLabelText("Field 2 type"), {
      target: { value: "array" },
    })
    fireEvent.change(screen.getByLabelText("Field 2 item type"), {
      target: { value: "integer" },
    })
    fireEvent.change(screen.getByLabelText("Field 2 default"), {
      target: { value: "80, 443" },
    })

    // Field 3: array of objects "routes" with child "path"
    fireEvent.change(screen.getByLabelText("Field 3 name"), {
      target: { value: "routes" },
    })
    fireEvent.change(screen.getByLabelText("Field 3 type"), {
      target: { value: "array" },
    })
    fireEvent.change(screen.getByLabelText("Field 3 item type"), {
      target: { value: "object" },
    })
    fireEvent.click(
      screen.getByRole("button", { name: "Add nested field to 3" })
    )
    fireEvent.change(screen.getByLabelText("Field 3.1 name"), {
      target: { value: "path" },
    })
    fireEvent.change(screen.getByLabelText("Field 3.1 default"), {
      target: { value: "/" },
    })

    const schemaTextarea = screen.getByLabelText(
      /JSON Schema/i
    ) as HTMLTextAreaElement
    const parsed = JSON.parse(schemaTextarea.value)
    expect(parsed.properties.db.type).toBe("object")
    expect(parsed.properties.db.properties.host.default).toBe("localhost")
    expect(parsed.properties.ports.type).toBe("array")
    expect(parsed.properties.ports.items).toEqual({ type: "integer" })
    expect(parsed.properties.ports.default).toEqual([80, 443])
    expect(parsed.properties.routes.type).toBe("array")
    expect(parsed.properties.routes.items.type).toBe("object")
    expect(parsed.properties.routes.items.properties.path.default).toBe("/")
  })

  it("shows validation error on invalid JSON Schema and supports editing a bundle item", async () => {
    const onNavigateToPreview = vi.fn()
    const bundleItem: CatalogItem = {
      id: "web-bundle",
      name: "Web Bundle",
      description: "Full web bundle",
      type: "bundle",
      category: "Platform",
      version: "1.0.0",
      schema: { type: "object", properties: {} },
      bundleItems: [
        { id: "db", kind: "infra", catalogRef: "postgres-db" },
        {
          id: "api",
          kind: "app",
          catalogRef: "test-app",
          dependsOn: ["db"],
          canary: true,
        },
      ],
    }
    vi.mocked(api.getCatalog).mockResolvedValue([...mockCatalog, bundleItem])
    vi.mocked(api.updateCatalogItem).mockResolvedValue({
      id: "301",
      title: "Update bundle web-bundle",
      branch: "keel/catalog/update-web-bundle-301",
      author: "user",
      tenant: "catalog",
      status: "open",
      createdAt: "just now",
      prUrl: "https://git.internal/pr/301",
    })

    render(<CatalogWizardView onNavigateToPreview={onNavigateToPreview} />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Web Bundle")).toBeInTheDocument()
    })

    // Click Edit on Web Bundle
    const bundleCard = screen
      .getByText("Web Bundle")
      .closest("div.bg-slate-900") as HTMLElement
    fireEvent.click(within(bundleCard).getByRole("button", { name: /^Edit$/i }))

    fireEvent.change(screen.getByLabelText(/Category \*/i), {
      target: { value: "Core Platform" },
    })
    fireEvent.change(screen.getByLabelText(/Description/i), {
      target: { value: "Updated bundle description" },
    })

    // Expand JSON Schema editor and enter invalid JSON
    fireEvent.click(screen.getByRole("button", { name: /Edit JSON Schema/i }))
    const schemaTextarea = screen.getByLabelText(/JSON Schema/i)
    fireEvent.change(schemaTextarea, { target: { value: "{invalid json" } })
    fireEvent.click(screen.getByRole("button", { name: /Submit Update PR/i }))

    expect(await screen.findByText(/Invalid JSON Schema:/i)).toBeInTheDocument()

    // Fix JSON Schema and submit
    fireEvent.change(schemaTextarea, {
      target: { value: '{"type":"object","properties":{}}' },
    })
    fireEvent.click(screen.getByRole("button", { name: /Submit Update PR/i }))

    await waitFor(() => {
      expect(api.updateCatalogItem).toHaveBeenCalledWith(
        "bundle",
        "web-bundle",
        expect.objectContaining({ name: "Web Bundle" })
      )
    })

    // Banner with Go to PR -> and View on Forge
    const goToPrBtns = await screen.findAllByRole("button", {
      name: /Go to PR/i,
    })
    fireEvent.click(goToPrBtns[0])
    expect(onNavigateToPreview).toHaveBeenCalled()
  })

  it("removes a bundle step and toggles Hide JSON Schema Editor", async () => {
    vi.mocked(api.getCatalog).mockResolvedValue(mockCatalog)
    render(<CatalogWizardView />)

    await waitFor(() => {
      const list = screen.getByTestId("catalog-list")
      expect(within(list).getByText("Test Microservice")).toBeInTheDocument()
    })

    fireEvent.click(screen.getByRole("button", { name: /^Add Catalog Item$/i }))
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "bundle" },
    })

    fireEvent.click(screen.getByRole("button", { name: /Add Step/i }))
    const step2Id = screen.getByLabelText("Step 2 ID")
    fireEvent.change(step2Id, { target: { value: "worker-step" } })
    expect((step2Id as HTMLInputElement).value).toBe("worker-step")
    fireEvent.change(screen.getByLabelText("Step 2 Kind"), {
      target: { value: "infra" },
    })
    fireEvent.change(screen.getByLabelText("Step 2 Kind"), {
      target: { value: "app" },
    })
    fireEvent.click(screen.getByRole("button", { name: "Remove step 2" }))
    expect(screen.queryByLabelText("Step 2 ID")).not.toBeInTheDocument()

    fireEvent.click(screen.getByRole("button", { name: /Edit JSON Schema/i }))
    expect(
      screen.getByRole("button", { name: /Hide JSON Schema Editor/i })
    ).toBeInTheDocument()
    fireEvent.click(
      screen.getByRole("button", { name: /Hide JSON Schema Editor/i })
    )
    expect(
      screen.getByRole("button", { name: /Edit JSON Schema/i })
    ).toBeInTheDocument()

    // Switch to infra and test Insert Starter Template + description
    fireEvent.change(screen.getByLabelText(/Catalog Type \*/i), {
      target: { value: "infra" },
    })
    fireEvent.change(screen.getByLabelText(/Description/i), {
      target: { value: "Starter infra" },
    })
    fireEvent.change(screen.getByLabelText(/Terraform Provider/i), {
      target: { value: "hashicorp/aws" },
    })
    fireEvent.click(
      screen.getByRole("button", { name: /Insert Starter Template/i })
    )
    expect(
      (screen.getByLabelText(/Pulumi\.yaml Program/i) as HTMLTextAreaElement)
        .value
    ).toContain("terraform-provider")
  })
})
