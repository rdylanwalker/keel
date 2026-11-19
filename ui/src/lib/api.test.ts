import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import {
  setStoredToken,
  getStoredToken,
  getAuthStatus,
  logout,
  getCatalog,
  getProposals,
  createProposal,
  getProposalPreview,
  mergeProposal,
  rejectProposal,
  reopenProposal,
  getStacks,
  getReleases,
  getTenants,
  createTenant,
  deleteTenant,
  createCatalogApp,
  updateCatalogItem,
  fetchCatalogSchema,
  getStackDiagnostics,
  getReleaseDiagnostics,
  getStackConfig,
  getReleaseConfig,
  deleteStack,
  deleteRelease,
  getBundleConfig,
  restartStackWorkspace,
  forceDeleteStack,
  reconcileRelease,
  getProviders,
  toggleProvider,
  getCoreAddons,
  upgradeManagementAddon,
  rollbackManagementAddon,
  createEventSourceStream,
  fetchStackSshInfo,
} from "./api"

describe("api client module", () => {
  beforeEach(() => {
    setStoredToken(null)
    vi.restoreAllMocks()
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it("handles stored token in memory without persisting to localStorage", () => {
    expect(getStoredToken()).toBeNull()
    setStoredToken("test-token-123")
    expect(getStoredToken()).toBe("test-token-123")
    expect(localStorage.getItem("keel_bootstrap_token")).toBeNull()
    setStoredToken(null)
    expect(getStoredToken()).toBeNull()
  })

  it("throws error on API HTTP failure with text response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 400,
        statusText: "Bad Request",
        text: async () => "Validation error occurred",
      })
    )

    await expect(getAuthStatus()).rejects.toThrow(
      "API error (400): Validation error occurred"
    )

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 502,
        statusText: "Bad Gateway",
        text: async () => {
          throw new Error("Text failed")
        },
      })
    )

    await expect(getAuthStatus()).rejects.toThrow(
      "API error (502): Bad Gateway"
    )
  })

  it("fetches auth status with Authorization header if token exists", async () => {
    setStoredToken("bearer-secret")
    const mockStatus = { authenticated: true, phase: "oidc-active" }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockStatus,
      })
    )

    const status = await getAuthStatus()
    expect(status).toEqual(mockStatus)
    expect(fetch).toHaveBeenCalledWith("/api/v1/auth/status", {
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer bearer-secret",
      },
    })
  })

  it("posts to /api/v1/auth/logout on logout", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ authenticated: false }),
    })
    vi.stubGlobal("fetch", fetchMock)

    const res = await logout()
    expect(res).toEqual({ authenticated: false })
    expect(fetchMock).toHaveBeenCalledWith("/api/v1/auth/logout", {
      method: "POST",
      headers: expect.objectContaining({
        "Content-Type": "application/json",
      }),
    })
  })

  it("fetches catalog", async () => {
    const mockCatalog = [{ id: "app-1", name: "Test App" }]
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockCatalog,
      })
    )

    const catalog = await getCatalog()
    expect(catalog).toEqual(mockCatalog)
    expect(fetch).toHaveBeenCalledWith("/api/v1/catalog", expect.any(Object))
  })

  it("fetches proposals with and without tenant filter", async () => {
    const mockProposals = [{ id: "10", title: "Prop 1" }]
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockProposals,
      })
    )

    const res1 = await getProposals()
    expect(res1).toMatchObject(mockProposals)
    expect(fetch).toHaveBeenCalledWith("/api/v1/proposals", expect.any(Object))

    await getProposals("tenant-acme")
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/proposals?tenant=tenant-acme",
      expect.any(Object)
    )
  })

  it("creates proposal", async () => {
    const payload = {
      catalogId: "app-web-service",
      tenant: "tenant-acme",
      values: { replicas: 3 },
    }
    const mockRes = { id: "100", title: "Deploy app" }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockRes,
      })
    )

    const res = await createProposal(payload)
    expect(res).toMatchObject(mockRes)
    expect(fetch).toHaveBeenCalledWith("/api/v1/proposals", {
      method: "POST",
      headers: expect.any(Object),
      body: JSON.stringify(payload),
    })
  })

  it("fetches proposal preview and merges proposal", async () => {
    const mockPreview = { resources: [], summary: { create: 1 } }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockPreview,
      })
    )

    const preview = await getProposalPreview("42")
    expect(preview).toEqual(mockPreview)
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/proposals/42/preview",
      expect.any(Object)
    )

    const mockMerge = { success: true }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockMerge,
      })
    )

    const mergeRes = await mergeProposal(42)
    expect(mergeRes).toEqual(mockMerge)
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/proposals/42/merge",
      expect.objectContaining({ method: "POST" })
    )

    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string) => {
        if (url.includes("/preview")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              url: "https://git.local/pr/7",
              report: {
                summary: { create: 1, update: 0, delete: 0, replace: 0 },
              },
            }),
          })
        }
        return Promise.resolve({
          ok: true,
          json: async () => ({ proposal: { id: "7", state: "merged" } }),
        })
      })
    )

    const wrappedPreview = await getProposalPreview(7)
    expect(wrappedPreview.prUrl).toBe("https://git.local/pr/7")
    expect(wrappedPreview.resources).toEqual([])

    const wrappedMerge = await mergeProposal(7)
    expect(wrappedMerge.success).toBe(true)

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ proposal: { id: "7", state: "closed" } }),
      })
    )
    const wrappedReject = await rejectProposal(7)
    expect(wrappedReject.success).toBe(true)

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ proposal: { id: "7", state: "open" } }),
      })
    )
    const wrappedReopen = await reopenProposal(7)
    expect(wrappedReopen.success).toBe(true)
  })

  it("fetches stacks and releases", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string) => {
        if (url.includes("/stacks")) {
          return Promise.resolve({
            ok: true,
            json: async () => [{ id: "stack-1" }],
          })
        }
        return Promise.resolve({
          ok: true,
          json: async () => [{ id: "rel-1" }],
        })
      })
    )

    const stacks = await getStacks()
    expect(stacks).toEqual([{ id: "stack-1" }])

    const releases = await getReleases()
    expect(releases).toEqual([{ id: "rel-1" }])
  })

  it("handles tenant management API operations and createCatalogApp", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string, init?: RequestInit) => {
        if (url.includes("/tenants") && init?.method === "POST") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              proposal: { id: "101", title: "Create Tenant PR" },
            }),
          })
        }
        if (url.includes("/tenants/") && init?.method === "DELETE") {
          return Promise.resolve({
            ok: true,
            json: async () => ({ id: "102", title: "Delete Tenant PR" }),
          })
        }
        if (url.includes("/tenants")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              tenants: [{ id: "default", namespace: "tenant-default" }],
            }),
          })
        }
        if (url.includes("/catalog") && init?.method === "POST") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              proposal: { id: "103", title: "Add Catalog App PR" },
            }),
          })
        }
        return Promise.resolve({ ok: false, status: 404 })
      })
    )

    const tenants = await getTenants()
    expect(tenants).toEqual([{ id: "default", namespace: "tenant-default" }])

    const createdTenant = await createTenant("acme")
    expect(createdTenant).toMatchObject({
      id: "101",
      title: "Create Tenant PR",
    })

    const deletedTenant = await deleteTenant("acme")
    expect(deletedTenant).toMatchObject({
      id: "102",
      title: "Delete Tenant PR",
    })

    const createdApp = await createCatalogApp({
      id: "my-app",
      name: "My App",
      category: "Applications",
      runtime: "helm",
      repoUrl: "https://charts.bitnami.com/bitnami",
      chart: "nginx",
      version: "1.0.0",
      flaggerSupported: true,
    })
    expect(createdApp).toMatchObject({ id: "103", title: "Add Catalog App PR" })
  })

  it("fetches providers and toggles provider enable state with wrapped and unwrapped payloads", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string, init?: RequestInit) => {
        if (url.includes("/providers/aws") && init?.method === "PUT") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              proposal: {
                id: "201",
                title: "Toggle AWS Provider",
                branch: "feature/toggle-aws",
              },
            }),
          })
        }
        if (url.includes("/providers/gcp") && init?.method === "PUT") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              id: "202",
              title: "Toggle GCP Provider",
              branch: "feature/toggle-gcp",
            }),
          })
        }
        if (url.includes("/providers")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              providers: [
                {
                  id: "aws",
                  enabled: true,
                  description: "Amazon Web Services",
                },
                {
                  id: "gcp",
                  enabled: false,
                  description: "Google Cloud Platform",
                },
              ],
            }),
          })
        }
        return Promise.resolve({ ok: false, status: 404 })
      })
    )

    const providers = await getProviders()
    expect(providers).toEqual([
      { id: "aws", enabled: true, description: "Amazon Web Services" },
      { id: "gcp", enabled: false, description: "Google Cloud Platform" },
    ])
    expect(fetch).toHaveBeenCalledWith("/api/v1/providers", expect.any(Object))

    const proposal1 = await toggleProvider("aws", false)
    expect(proposal1).toEqual({
      id: "201",
      title: "Toggle AWS Provider",
      branch: "feature/toggle-aws",
    })

    const proposal2 = await toggleProvider("gcp", true)
    expect(proposal2).toEqual({
      id: "202",
      title: "Toggle GCP Provider",
      branch: "feature/toggle-gcp",
    })

    // Array response for getProviders
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => [{ id: "hetzner", enabled: true }],
      })
    )
    const arrayProviders = await getProviders()
    expect(arrayProviders).toEqual([{ id: "hetzner", enabled: true }])

    // Empty object response fallback for getProviders
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({}),
      })
    )
    const emptyProviders = await getProviders()
    expect(emptyProviders).toEqual([])
  })

  it("unwraps keel-server object envelopes for catalog, proposals, stacks, and releases", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string, init?: RequestInit) => {
        if (url.includes("/catalog")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              applications: [
                {
                  metadata: {
                    id: "podinfo",
                    name: "Podinfo",
                    category: "apps",
                  },
                  spec: {
                    upstream: { chart: "podinfo", version: "6.7.0" },
                    schema: { type: "object", properties: {} },
                  },
                },
                { metadata: {}, spec: {} },
              ],
              blueprints: [
                {
                  metadata: {
                    id: "kind-cluster",
                    name: "Kind Cluster",
                    category: "infra",
                    provider: "kind",
                  },
                  spec: { schema: { type: "object", properties: {} } },
                },
                { metadata: {}, spec: {} },
              ],
            }),
          })
        }
        if (url.includes("/proposals") && init?.method === "POST") {
          return Promise.resolve({
            ok: true,
            json: async () => ({ proposal: { id: "99", title: "Wrapped PR" } }),
          })
        }
        if (url.includes("/proposals")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({ proposals: [{ id: "99", title: "Wrapped" }] }),
          })
        }
        if (url.includes("/stacks")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({ stacks: [{ id: "s-1" }] }),
          })
        }
        return Promise.resolve({
          ok: true,
          json: async () => ({ releases: [{ id: "r-1" }] }),
        })
      })
    )

    const catalog = await getCatalog()
    expect(catalog).toHaveLength(4)
    expect(catalog[0].id).toBe("kind-cluster")
    expect(catalog[0].type).toBe("infra")
    expect(catalog[2].id).toBe("podinfo")
    expect(catalog[2].type).toBe("app")

    const proposals = await getProposals()
    expect(proposals).toMatchObject([{ id: "99", title: "Wrapped" }])

    const created = await createProposal({
      catalogId: "kind-cluster",
      tenant: "default",
      values: {},
    })
    expect(created).toMatchObject({ id: "99", title: "Wrapped PR" })

    const stacks = await getStacks()
    expect(stacks).toEqual([{ id: "s-1" }])

    const releases = await getReleases()
    expect(releases).toEqual([{ id: "r-1" }])
  })

  it("creates EventSource stream helper and processes events", () => {
    let messageHandler: ((e: MessageEvent) => void) | undefined
    let errorHandler: ((e: Event) => void) | undefined
    const closeSpy = vi.fn()

    class MockEventSource {
      url: string
      constructor(url: string) {
        this.url = url
      }
      set onmessage(fn: (e: MessageEvent) => void) {
        messageHandler = fn
      }
      set onerror(fn: (e: Event) => void) {
        errorHandler = fn
      }
      addEventListener(type: string, fn: (e: MessageEvent) => void) {
        if (type === "crd-watch") {
          messageHandler = fn
        }
      }
      close() {
        closeSpy()
      }
    }

    vi.stubGlobal("EventSource", MockEventSource)

    const onEvent = vi.fn()
    const onError = vi.fn()

    const es = createEventSourceStream(onEvent, onError)
    expect(es).toBeInstanceOf(MockEventSource)

    const mockEvent = {
      kind: "Kustomization",
      namespace: "tenant-default",
      name: "app",
      status: "Ready",
      message: "Synced",
      timestamp: "12:00:00",
    }

    // Trigger message event
    if (messageHandler) {
      messageHandler({ data: JSON.stringify(mockEvent) } as MessageEvent)
    }
    expect(onEvent).toHaveBeenCalledWith(mockEvent)

    // Invalid JSON should not throw
    if (messageHandler) {
      messageHandler({ data: "invalid json" } as MessageEvent)
    }

    // Trigger error event
    const err = new Event("error")
    if (errorHandler) {
      errorHandler(err)
    }
    expect(onError).toHaveBeenCalledWith(err)
  })

  it("updates a catalog item via PUT /api/v1/catalog/{kind}/{id}", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({
        proposal: {
          id: "305",
          title: "feat(catalog): update infra kind-cluster",
          sourceBranch: "keel/catalog/update-kind-cluster-305",
          state: "open",
        },
      }),
    })
    vi.stubGlobal("fetch", fetchMock)

    const proposal = await updateCatalogItem("infrastructure", "kind-cluster", {
      name: "kind Cluster v2",
      category: "Infrastructure",
      description: "Updated kind blueprint",
      provider: "kind",
      pulumiRuntime: "yaml",
      entrypoint: "Pulumi.yaml",
      protectByDefault: true,
    })

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/catalog/infra/kind-cluster",
      expect.objectContaining({
        method: "PUT",
      })
    )
    expect(proposal).toMatchObject({
      id: "305",
      title: "feat(catalog): update infra kind-cluster",
      branch: "keel/catalog/update-kind-cluster-305",
    })
  })

  it("calls stack, release, bundle, and schema diagnostic/config/lifecycle endpoints", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({
        ok: true,
        proposal: { id: "1", title: "delete" },
      }),
    })
    vi.stubGlobal("fetch", fetchMock)

    await fetchCatalogSchema({
      repoUrl: "https://charts.example.com",
      chart: "podinfo",
      version: "6.5.0",
    })
    await getStackDiagnostics("tenant-acme", "dev-kind")
    await getReleaseDiagnostics("tenant-acme", "podinfo")
    await getStackConfig("tenant-acme", "dev-kind")
    await getReleaseConfig("tenant-acme", "podinfo")
    await deleteStack("tenant-acme", "dev-kind")
    await deleteRelease("tenant-acme", "podinfo")
    await getBundleConfig("tenant-acme", "kind-with-podinfo")
    await restartStackWorkspace("tenant-acme", "dev-kind")
    await forceDeleteStack("tenant-acme", "dev-kind")
    await reconcileRelease("tenant-acme", "podinfo")

    expect(fetchMock).toHaveBeenCalledTimes(11)
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/stacks/tenant-acme/dev-kind/force-delete",
      expect.objectContaining({ method: "POST" })
    )
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/catalog/fetch-schema",
      expect.objectContaining({ method: "POST" })
    )
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/stacks/tenant-acme/dev-kind/diagnostics",
      expect.any(Object)
    )
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/releases/tenant-acme/podinfo/reconcile",
      expect.objectContaining({ method: "POST" })
    )
  })

  it("dispatches keel:unauthorized event when an API request returns 401", async () => {
    const onUnauthorized = vi.fn()
    window.addEventListener("keel:unauthorized", onUnauthorized)

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 401,
        statusText: "Unauthorized",
        text: async () => "Authentication session has expired",
      })
    )

    await expect(getStacks()).rejects.toThrow(
      "API error (401): Authentication session has expired"
    )
    expect(onUnauthorized).toHaveBeenCalledTimes(1)

    window.removeEventListener("keel:unauthorized", onUnauthorized)
  })

  it("fetches coreAddons and core_addons from /api/v1/catalog", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValueOnce({
          ok: true,
          json: async () => ({
            coreAddons: [
              {
                metadata: { id: "kyverno", name: "Kyverno" },
                spec: {
                  upstream: {
                    repoURL: "https://kyverno.github.io/kyverno/",
                    chart: "kyverno",
                    version: "3.3.4",
                  },
                  schema: { type: "object", properties: {} },
                },
              },
            ],
          }),
        })
        .mockResolvedValueOnce({
          ok: true,
          json: async () => ({
            core_addons: [
              {
                metadata: { id: "flagger", name: "Flagger" },
                spec: {
                  upstream: {
                    repoURL: "https://flagger.app",
                    chart: "flagger",
                    version: "1.40.0",
                  },
                },
              },
            ],
          }),
        })
        .mockResolvedValueOnce({
          ok: true,
          json: async () => [],
        })
    )

    const addons1 = await getCoreAddons()
    expect(addons1).toHaveLength(1)
    expect(addons1[0]).toMatchObject({
      id: "kyverno",
      name: "Kyverno",
      repoUrl: "https://kyverno.github.io/kyverno/",
      chart: "kyverno",
      version: "3.3.4",
    })

    const addons2 = await getCoreAddons()
    expect(addons2).toHaveLength(1)
    expect(addons2[0]).toMatchObject({
      id: "flagger",
      chart: "flagger",
      version: "1.40.0",
    })

    const addons3 = await getCoreAddons()
    expect(addons3).toEqual([])
  })

  it("calls upgradeManagementAddon and rollbackManagementAddon endpoints", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({
        proposal: {
          id: "401",
          title: "chore(mgmt): upgrade kyverno",
          branch: "keel/mgmt/upgrade-kyverno",
          state: "open",
        },
      }),
    })
    vi.stubGlobal("fetch", fetchMock)

    const upgradeProposal = await upgradeManagementAddon("kyverno", {
      version: "3.3.5",
      values: { replicaCount: 3 },
    })
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/management/addons/kyverno/upgrade",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({
          version: "3.3.5",
          values: { replicaCount: 3 },
        }),
      })
    )
    expect(upgradeProposal).toMatchObject({
      id: "401",
      branch: "keel/mgmt/upgrade-kyverno",
    })

    const rollbackProposal = await rollbackManagementAddon("kyverno")
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/management/addons/kyverno/rollback",
      expect.objectContaining({ method: "POST" })
    )
    expect(rollbackProposal).toMatchObject({
      id: "401",
    })
  })

  it("fetches stack SSH info from /api/v1/stacks/{ns}/{name}/ssh-info", async () => {
    const mockSshInfo = {
      stackName: "pve-workload",
      namespace: "tenant-acme",
      tenant: "acme",
      provider: "proxmox",
      sshHost: "192.168.1.150",
      sshUser: "debian",
      sshPort: 22,
      sshCommand: "ssh debian@192.168.1.150 -p 22",
      cliCommand: "keel infra ssh pve-workload --tenant acme",
      cliExecCommand: "keel infra ssh pve-workload --tenant acme --exec",
      hasSavedSshKey: true,
    }

    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => mockSshInfo,
    })
    vi.stubGlobal("fetch", fetchMock)

    const info = await fetchStackSshInfo("tenant-acme", "pve-workload")
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/stacks/tenant-acme/pve-workload/ssh-info",
      expect.any(Object)
    )
    expect(info).toEqual(mockSshInfo)
  })
})
