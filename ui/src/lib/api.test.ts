import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import {
  setStoredToken,
  getStoredToken,
  bootstrapLogin,
  getAuthStatus,
  getCatalog,
  getProposals,
  createProposal,
  getProposalPreview,
  mergeProposal,
  rejectProposal,
  getStacks,
  getReleases,
  getTenants,
  createTenant,
  deleteTenant,
  createCatalogApp,
  updateCatalogItem,
  getProviders,
  toggleProvider,
  createEventSourceStream,
} from "./api"

describe("api client module", () => {
  beforeEach(() => {
    localStorage.clear()
    vi.restoreAllMocks()
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it("handles stored token in localStorage", () => {
    expect(getStoredToken()).toBeNull()
    setStoredToken("test-token-123")
    expect(getStoredToken()).toBe("test-token-123")
    setStoredToken(null)
    expect(getStoredToken()).toBeNull()
  })

  it("handles storage exceptions gracefully", () => {
    const getItemSpy = vi
      .spyOn(Storage.prototype, "getItem")
      .mockImplementation(() => {
        throw new Error("Storage denied")
      })
    expect(getStoredToken()).toBeNull()

    const setItemSpy = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw new Error("Storage denied")
      })
    expect(() => setStoredToken("token")).not.toThrow()

    getItemSpy.mockRestore()
    setItemSpy.mockRestore()
  })

  it("calls bootstrapLogin successfully and saves token", async () => {
    const mockResponse = { success: true, token: "returned-token-abc" }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockResponse,
      })
    )

    const result = await bootstrapLogin("my-token")
    expect(result).toEqual(mockResponse)
    expect(getStoredToken()).toBe("returned-token-abc")
    expect(fetch).toHaveBeenCalledWith("/api/v1/auth/bootstrap-login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token: "my-token" }),
    })
  })

  it("does not store raw single-use bootstrap token when server uses HttpOnly session cookie", async () => {
    const mockResponse = { authenticated: true }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => mockResponse,
      })
    )

    await bootstrapLogin("single-use-token")
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

    await expect(bootstrapLogin("invalid")).rejects.toThrow(
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

    await expect(bootstrapLogin("invalid")).rejects.toThrow(
      "API error (502): Bad Gateway"
    )
  })

  it("fetches auth status with Authorization header if token exists", async () => {
    setStoredToken("bearer-secret")
    const mockStatus = { authenticated: true, phase: "bootstrap" }
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
})
