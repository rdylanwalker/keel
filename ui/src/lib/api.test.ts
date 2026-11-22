import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"
import {
  getAuthStatus,
  logout,
  getCatalog,
  getProposals,
  getProposalListing,
  proposalKey,
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
  restartStackWorkspace,
  reconcileStack,
  forceDeleteStack,
  unlockStack,
  reconcileRelease,
  getProviders,
  toggleProvider,
  getCoreAddons,
  upgradeManagementAddon,
  rollbackManagementAddon,
  createEventSourceStream,
  fetchStackSshInfo,
  startOidcLogin,
} from "./api"

/** A wire proposal as the server sends it (`ScopedProposal`: forge PR + scope + repository). */
function wireProposal(overrides: Record<string, unknown> = {}) {
  return {
    id: "1",
    title: "PR",
    body: "",
    state: "open",
    sourceBranch: "keel/branch",
    targetBranch: "main",
    headSha: "0000000",
    tenant: null,
    url: "local:///tmp/remote.git/.keel/proposals/1.json",
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    comments: [],
    scope: "management",
    repository: { label: "platform" },
    ...overrides,
  }
}

describe("api client module", () => {
  beforeEach(() => {
    vi.restoreAllMocks()
  })

  afterEach(() => {
    vi.restoreAllMocks()
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

    await expect(getAuthStatus()).rejects.toThrow("Validation error occurred")

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
      "The Keel server could not complete the request (HTTP 502 Bad Gateway)."
    )
  })

  it("fetches auth status relying on the session cookie (no Authorization header)", async () => {
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
      headers: { "Content-Type": "application/json" },
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
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          applications: [],
          blueprints: [],
          bundles: [],
          enabledProviders: [],
        }),
      })
    )

    const catalog = await getCatalog()
    expect(catalog).toEqual([])
    expect(fetch).toHaveBeenCalledWith("/api/v1/catalog", expect.any(Object))
  })

  it("fetches the aggregated listing or one tenant's proposals", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          proposals: [
            wireProposal({ id: "10", title: "Prop 1", scope: "acme" }),
          ],
        }),
      })
    )

    const res1 = await getProposals()
    expect(res1).toMatchObject([{ id: "10", title: "Prop 1", scope: "acme" }])
    expect(fetch).toHaveBeenCalledWith("/api/v1/proposals", expect.any(Object))

    await getProposals("acme", "all")
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/tenants/acme/proposals?status=all",
      expect.any(Object)
    )

    await getProposals(undefined, "merged")
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/proposals?status=merged",
      expect.any(Object)
    )
  })

  it("rejects proposals without a route scope instead of guessing one", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          proposals: [{ id: "3", title: "No scope", tenant: "acme" }],
        }),
      })
    )
    await expect(getProposals()).rejects.toThrow(/has no scope/)
  })

  it("maps scoped proposals with repository identity and unavailable repositories", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          proposals: [
            {
              id: "3",
              title: "acme app",
              sourceBranch: "keel/acme/podinfo-1",
              tenant: "acme",
              state: "open",
              url: "https://forge.example/keel-tenants/infra-tenant-acme/pulls/3",
              scope: "acme",
              repository: {
                label: "tenant-acme",
                fullName: "keel-tenants/infra-tenant-acme",
              },
            },
            {
              id: "3",
              title: "platform change",
              sourceBranch: "keel/providers/toggle-aws-1",
              tenant: null,
              state: "merged",
              url: "local:///tmp/remote.git/.keel/proposals/3.json",
              scope: "management",
              repository: { label: "platform" },
            },
          ],
          unavailable: [
            {
              scope: "globex",
              repository: { label: "tenant-globex" },
              reason: "forge unavailable",
            },
          ],
        }),
      })
    )
    const listing = await getProposalListing()
    expect(listing.proposals[0]).toMatchObject({
      id: "3",
      scope: "acme",
      tenant: "acme",
      branch: "keel/acme/podinfo-1",
      repository: {
        label: "tenant-acme",
        fullName: "keel-tenants/infra-tenant-acme",
      },
      repoUrl: "https://forge.example/keel-tenants/infra-tenant-acme",
    })
    // Platform PRs without a tenant keep the platform scope; local:// URLs give no repo link.
    expect(listing.proposals[1]).toMatchObject({
      scope: "management",
      tenant: "management",
      status: "merged",
    })
    expect(listing.proposals[1].repoUrl).toBeUndefined()
    expect(proposalKey(listing.proposals[0])).toBe("acme/3")
    expect(proposalKey(listing.proposals[1])).toBe("management/3")
    expect(listing.unavailable).toEqual([
      {
        scope: "globex",
        repository: { label: "tenant-globex" },
        reason: "forge unavailable",
      },
    ])
  })

  it("creates proposal", async () => {
    const payload = {
      catalogId: "podinfo",
      tenant: "tenant-acme",
      values: { replicaCount: 3 },
    }
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          proposal: wireProposal({
            id: "100",
            title: "Deploy app",
            scope: "tenant-acme",
            sourceBranch: "keel/tenant-acme/podinfo-1",
          }),
          branch: "keel/tenant-acme/podinfo-1",
        }),
      })
    )

    const res = await createProposal(payload)
    expect(res).toMatchObject({
      id: "100",
      title: "Deploy app",
      scope: "tenant-acme",
      branch: "keel/tenant-acme/podinfo-1",
      status: "open",
    })
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/tenants/tenant-acme/proposals",
      {
        method: "POST",
        headers: expect.any(Object),
        body: JSON.stringify(payload),
      }
    )
  })

  it("fetches proposal preview and merges proposal", async () => {
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
                source: "synthesized",
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

    const wrappedPreview = await getProposalPreview({ scope: "acme", id: "7" })
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/tenants/acme/proposals/7/preview",
      expect.any(Object)
    )
    expect(wrappedPreview.prUrl).toBe("https://git.local/pr/7")
    expect(wrappedPreview.resources).toEqual([])
    // Absent policy status must not be reported as "passed".
    expect(wrappedPreview.policyStatus).toBeUndefined()
    expect(wrappedPreview.source).toBe("synthesized")

    const wrappedMerge = await mergeProposal({ scope: "acme", id: "7" })
    expect(wrappedMerge).toEqual({
      success: true,
      message: "PR #7 merged cleanly!",
    })
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/tenants/acme/proposals/7/merge",
      expect.objectContaining({ method: "POST" })
    )

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ proposal: { id: "7", state: "closed" } }),
      })
    )
    const wrappedReject = await rejectProposal({ scope: "acme", id: "7" })
    expect(wrappedReject.success).toBe(true)

    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ proposal: { id: "7", state: "open" } }),
      })
    )
    const wrappedReopen = await reopenProposal({ scope: "acme", id: "7" })
    expect(wrappedReopen.success).toBe(true)
  })

  it("handles tenant management API operations and createCatalogApp", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string, init?: RequestInit) => {
        if (url.includes("/tenants") && init?.method === "POST") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              tenant: { id: "acme", namespace: "tenant-acme" },
              proposal: wireProposal({
                id: "101",
                title: "Create Tenant PR",
                sourceBranch: "keel/acme/create-tenant-1",
              }),
              branch: "keel/acme/create-tenant-1",
              repository: {
                layout: "dedicated",
                owner: "keel-tenants",
                name: "infra-tenant-acme",
                webUrl: "javascript:alert(1)",
                transport: "https",
                skeletonProposal: wireProposal({
                  id: "1",
                  title: "skeleton",
                  scope: "acme",
                  tenant: "acme",
                  repository: {
                    label: "tenant-acme",
                    fullName: "keel-tenants/infra-tenant-acme",
                  },
                  url: "https://forge.example/keel-tenants/infra-tenant-acme/pulls/1",
                }),
                notes: [],
                cluster: {
                  namespaceReady: false,
                  sopsAge: "pendingNamespace",
                  credential: "pendingNamespace",
                },
              },
            }),
          })
        }
        if (url.includes("/tenants/") && init?.method === "DELETE") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              id: "acme",
              proposal: wireProposal({
                id: "102",
                title: "Delete Tenant PR",
                sourceBranch: "keel/acme/delete-tenant-2",
              }),
              branch: "keel/acme/delete-tenant-2",
              notes: ["revoke credentials"],
            }),
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
              proposal: wireProposal({
                id: "103",
                title: "Add Catalog App PR",
              }),
              branch: "keel/branch",
            }),
          })
        }
        return Promise.resolve({ ok: false, status: 404 })
      })
    )

    const tenants = await getTenants()
    expect(tenants).toEqual([{ id: "default", namespace: "tenant-default" }])

    const createdTenant = await createTenant("acme")
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/tenants",
      expect.objectContaining({ body: JSON.stringify({ id: "acme" }) })
    )
    expect(createdTenant.tenant).toEqual({
      id: "acme",
      namespace: "tenant-acme",
    })
    expect(createdTenant.proposal).toMatchObject({
      id: "101",
      title: "Create Tenant PR",
      scope: "management",
      branch: "keel/acme/create-tenant-1",
    })
    expect(createdTenant.repository).toMatchObject({
      layout: "dedicated",
      transport: "https",
      notes: [],
      skeletonProposal: {
        id: "1",
        scope: "acme",
        repoUrl: "https://forge.example/keel-tenants/infra-tenant-acme",
      },
    })
    // Never render a non-http(s) repository link.
    expect(createdTenant.repository.webUrl).toBeUndefined()

    await createTenant("acme", { adoptExisting: true })
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/tenants",
      expect.objectContaining({
        body: JSON.stringify({ id: "acme", adoptExisting: true }),
      })
    )

    const deletedTenant = await deleteTenant("acme")
    expect(deletedTenant).toMatchObject({
      id: "acme",
      branch: "keel/acme/delete-tenant-2",
      notes: ["revoke credentials"],
      proposal: {
        id: "102",
        title: "Delete Tenant PR",
        branch: "keel/acme/delete-tenant-2",
      },
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

  it("fetches providers and toggles provider enable state", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation((url: string, init?: RequestInit) => {
        if (url.includes("/providers/aws") && init?.method === "PUT") {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              provider: { id: "aws", enabled: false },
              proposal: wireProposal({
                id: "201",
                title: "Toggle AWS Provider",
                sourceBranch: "feature/toggle-aws",
              }),
              branch: "feature/toggle-aws",
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
    expect(proposal1).toMatchObject({
      id: "201",
      title: "Toggle AWS Provider",
      branch: "feature/toggle-aws",
      scope: "management",
    })
  })

  it("maps spec.placement.scope: cluster onto placementScope", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({
          applications: [
            {
              metadata: { id: "cert-manager", name: "cert-manager" },
              spec: {
                upstream: { chart: "cert-manager" },
                placement: { scope: "cluster" },
              },
            },
            {
              metadata: { id: "flux2", name: "Flux CD" },
              spec: {
                placement: { scope: "cluster", namespace: "flux-system" },
              },
            },
            {
              metadata: { id: "podinfo", name: "Podinfo" },
              spec: {
                placement: { scope: "namespaced", namespace: "ignored" },
              },
            },
            { metadata: { id: "dex", name: "Dex" }, spec: {} },
          ],
        }),
      })
    )
    const catalog = await getCatalog()
    expect(
      catalog.map((c) => [c.id, c.placementScope, c.placementNamespace])
    ).toEqual([
      ["cert-manager", "cluster", undefined],
      ["flux2", "cluster", "flux-system"],
      ["podinfo", undefined, undefined],
      ["dex", undefined, undefined],
    ])
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
            json: async () => ({
              proposal: wireProposal({
                id: "99",
                title: "Wrapped PR",
                scope: "acme",
              }),
              branch: "keel/branch",
            }),
          })
        }
        if (url.includes("/proposals")) {
          return Promise.resolve({
            ok: true,
            json: async () => ({
              proposals: [
                wireProposal({ id: "99", title: "Wrapped", scope: "acme" }),
              ],
            }),
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
        proposal: wireProposal({
          id: "305",
          title: "feat(catalog): update infra kind-cluster",
          sourceBranch: "keel/catalog/update-kind-cluster-305",
        }),
        branch: "keel/catalog/update-kind-cluster-305",
      }),
    })
    vi.stubGlobal("fetch", fetchMock)

    const proposal = await updateCatalogItem("infra", "kind-cluster", {
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

  it("calls stack, release, and schema diagnostic/config/lifecycle endpoints", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({
        id: "x",
        namespace: "tenant-acme",
        branch: "keel/acme/delete",
        proposal: wireProposal({ id: "1", title: "delete", scope: "acme" }),
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
    await restartStackWorkspace("tenant-acme", "dev-kind")
    await forceDeleteStack("tenant-acme", "dev-kind")
    await unlockStack("tenant-acme", "dev-kind")
    await reconcileStack("tenant-acme", "dev-kind")
    await reconcileRelease("tenant-acme", "podinfo")

    expect(fetchMock).toHaveBeenCalledTimes(12)
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/v1/stacks/tenant-acme/dev-kind/unlock",
      expect.objectContaining({ method: "POST" })
    )
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
      "Authentication session has expired"
    )
    expect(onUnauthorized).toHaveBeenCalledTimes(1)

    window.removeEventListener("keel:unauthorized", onUnauthorized)
  })

  it("fetches coreAddons from /api/v1/catalog", async () => {
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
          // `coreAddons` is omitted when empty.
          json: async () => ({ applications: [], blueprints: [], bundles: [] }),
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
    expect(addons2).toEqual([])
  })

  it("calls upgradeManagementAddon and rollbackManagementAddon endpoints", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({
        id: "kyverno",
        proposal: wireProposal({
          id: "401",
          title: "chore(mgmt): upgrade kyverno",
          sourceBranch: "keel/mgmt/upgrade-kyverno",
        }),
        branch: "keel/mgmt/upgrade-kyverno",
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

  it("maps reject/reopen proposal states and fetchStackSshInfo demo mode", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValueOnce({
          ok: true,
          json: async () => ({ proposal: { id: "9", state: "open" } }),
        })
        .mockResolvedValueOnce({
          ok: true,
          json: async () => ({ proposal: { id: "9", state: "closed" } }),
        })
    )

    await expect(reopenProposal({ scope: "acme", id: "9" })).resolves.toEqual({
      success: true,
      message: "PR #9 reopened.",
    })
    await expect(rejectProposal({ scope: "acme", id: "9" })).resolves.toEqual({
      success: true,
      message: "PR #9 rejected and closed.",
    })

    window.history.replaceState({}, "", "/?demo=true")
    const demoInfo = await fetchStackSshInfo("tenant-acme", "azure-vm")
    expect(demoInfo.tenant).toBe("acme")
    expect(demoInfo.sshHost).toBe("192.168.1.150")

    const customNsInfo = await fetchStackSshInfo("custom-ns", "azure-vm")
    expect(customNsInfo.tenant).toBe("custom-ns")
    window.history.replaceState({}, "", "/")

    // Non-demo fetch rejection propagates
    vi.stubGlobal(
      "fetch",
      vi.fn().mockRejectedValueOnce(new Error("SSH info network error"))
    )
    await expect(fetchStackSshInfo("tenant-acme", "azure-vm")).rejects.toThrow(
      "SSH info network error"
    )

    // Bundle with empty items & no description
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValueOnce({
        ok: true,
        json: async () => ({
          bundles: [{ metadata: { id: "empty-bundle" }, spec: { items: [] } }],
          blueprints: [],
          applications: [],
        }),
      })
    )
    const catalog = await getCatalog()
    expect(catalog[0].description).toBe("Multi-item catalog bundle")

    // startOidcLogin delegates to window.location.assign
    const assignMock = vi.fn()
    const origLocation = window.location
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { ...origLocation, assign: assignMock },
    })
    startOidcLogin()
    expect(assignMock).toHaveBeenCalledWith("/api/v1/auth/login")
    Object.defineProperty(window, "location", {
      configurable: true,
      value: origLocation,
    })
  })
})
