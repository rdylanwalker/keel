import { RJSFSchema } from "@rjsf/utils"
import {
  AuthStatus,
  CatalogBundleItem,
  CatalogItem,
  CreateCatalogItemPayload,
  CreateProposalPayload,
  CrdWatchEvent,
  PreviewReport,
  Proposal,
  ProviderRegistryEntry,
  ReleaseConfig,
  ReleaseSummary,
  RestartStackResponse,
  StackDiagnostics,
  StackSummary,
  TenantSummary,
  UpgradeManagementAddonPayload,
} from "../types/api"

let inMemoryToken: string | null = null

export function getStoredToken(): string | null {
  return inMemoryToken
}

export function setStoredToken(token: string | null): void {
  inMemoryToken = token ? token : null
}

function getHeaders(
  customHeaders?: Record<string, string>
): Record<string, string> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...customHeaders,
  }
  const token = getStoredToken()
  if (token) {
    headers["Authorization"] = `Bearer ${token}`
  }
  return headers
}

async function handleResponse<T>(res: Response): Promise<T> {
  if (!res.ok) {
    if (res.status === 401 && typeof window !== "undefined") {
      window.dispatchEvent(new CustomEvent("keel:unauthorized"))
    }
    let errorText = res.statusText
    try {
      if (typeof res.text === "function") {
        const text = await res.text()
        if (text) errorText = text
      }
    } catch {
      // fallback to statusText
    }
    throw new Error(`API error (${res.status}): ${errorText || res.statusText}`)
  }
  return res.json() as Promise<T>
}

export function startOidcLogin(): void {
  window.location.assign("/api/v1/auth/login")
}

export async function getAuthStatus(): Promise<AuthStatus> {
  const res = await fetch("/api/v1/auth/status", {
    headers: getHeaders(),
  })
  return handleResponse<AuthStatus>(res)
}

interface RawCatalogEntry {
  metadata?: {
    id?: string
    name?: string
    description?: string
    category?: string
    runtime?: string
    provider?: string
  }
  spec?: {
    upstream?: {
      repoURL?: string
      chart?: string
      version?: string
      path?: string
    }
    progressiveDelivery?: {
      flaggerSupported?: boolean
      defaultCanary?: boolean
    }
    pulumiRuntime?: "yaml" | "hcl" | "opentofu-bridge"
    entrypoint?: string
    protectByDefault?: boolean
    serviceUrl?: string
    schema?: Record<string, unknown>
    uiSchema?: Record<string, unknown>
    items?: CatalogBundleItem[]
  }
}

interface RawCatalogResponse {
  applications?: RawCatalogEntry[]
  blueprints?: RawCatalogEntry[]
  bundles?: RawCatalogEntry[]
  coreAddons?: RawCatalogEntry[]
  core_addons?: RawCatalogEntry[]
  pulumiYaml?: Record<string, string>
}

function mapCatalogApplication(app: RawCatalogEntry): CatalogItem {
  return {
    id: app.metadata?.id ?? "unknown-app",
    name: app.metadata?.name ?? app.metadata?.id ?? "Application",
    description:
      app.metadata?.description ||
      (app.spec?.upstream?.chart
        ? `Helm chart: ${app.spec.upstream.chart}`
        : "Upstream Kubernetes application"),
    type: "app",
    category: app.metadata?.category ?? "Applications",
    version: app.spec?.upstream?.version ?? "0.1.0",
    repoUrl: app.spec?.upstream?.repoURL,
    chart: app.spec?.upstream?.chart,
    path: app.spec?.upstream?.path,
    runtime: app.metadata?.runtime,
    flaggerSupported: app.spec?.progressiveDelivery?.flaggerSupported ?? false,
    defaultCanary: app.spec?.progressiveDelivery?.defaultCanary ?? false,
    serviceUrl: app.spec?.serviceUrl,
    schema: app.spec?.schema ?? { type: "object", properties: {} },
    uiSchema: app.spec?.uiSchema,
  }
}

export async function getCatalog(): Promise<CatalogItem[]> {
  const res = await fetch("/api/v1/catalog", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<CatalogItem[] | RawCatalogResponse>(res)
  if (Array.isArray(raw)) {
    return raw
  }
  const apps: CatalogItem[] = (raw.applications ?? []).map(
    mapCatalogApplication
  )
  const blueprints: CatalogItem[] = (raw.blueprints ?? []).map((bp) => ({
    id: bp.metadata?.id ?? "unknown-blueprint",
    name: bp.metadata?.name ?? bp.metadata?.id ?? "Infrastructure Blueprint",
    description:
      bp.metadata?.description ||
      (bp.metadata?.provider
        ? `PKO v2 infrastructure blueprint (${bp.metadata.provider})`
        : "PKO v2 infrastructure blueprint"),
    type: "infra",
    category: bp.metadata?.category ?? "Infrastructure",
    provider: bp.metadata?.provider,
    version: "1.0.0",
    pulumiRuntime: bp.spec?.pulumiRuntime ?? "yaml",
    entrypoint: bp.spec?.entrypoint ?? "Pulumi.yaml",
    protectByDefault: bp.spec?.protectByDefault ?? false,
    pulumiYaml: raw.pulumiYaml?.[bp.metadata?.id ?? ""],
    schema: bp.spec?.schema ?? { type: "object", properties: {} },
    uiSchema: bp.spec?.uiSchema,
  }))
  const bundles: CatalogItem[] = (raw.bundles ?? []).map((b) => ({
    id: b.metadata?.id ?? "unknown-bundle",
    name: b.metadata?.name ?? b.metadata?.id ?? "Catalog Bundle",
    description:
      b.metadata?.description ||
      (b.spec?.items?.length
        ? `Multi-item bundle (${b.spec.items.map((i) => i.id).join(" → ")})`
        : "Multi-item catalog bundle"),
    type: "bundle",
    category: b.metadata?.category ?? "Platform",
    version: "bundle",
    bundleItems: b.spec?.items ?? [],
    schema: b.spec?.schema ?? { type: "object", properties: {} },
    uiSchema: b.spec?.uiSchema,
  }))
  return [...bundles, ...blueprints, ...apps]
}

export async function getCoreAddons(): Promise<CatalogItem[]> {
  const res = await fetch("/api/v1/catalog", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<CatalogItem[] | RawCatalogResponse>(res)
  if (Array.isArray(raw)) {
    return []
  }
  const coreAddons = raw.coreAddons ?? raw.core_addons ?? []
  return coreAddons.map(mapCatalogApplication)
}

export async function upgradeManagementAddon(
  id: string,
  payload: UpgradeManagementAddonPayload
): Promise<Proposal> {
  const res = await fetch(
    `/api/v1/management/addons/${encodeURIComponent(id)}/upgrade`,
    {
      method: "POST",
      headers: getHeaders(),
      body: JSON.stringify(payload),
    }
  )
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export async function rollbackManagementAddon(id: string): Promise<Proposal> {
  const res = await fetch(
    `/api/v1/management/addons/${encodeURIComponent(id)}/rollback`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export async function fetchCatalogSchema(payload: {
  repoUrl: string
  chart: string
  version?: string
}): Promise<{ schema: RJSFSchema; source: string }> {
  const res = await fetch("/api/v1/catalog/fetch-schema", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify(payload),
  })
  return handleResponse<{ schema: RJSFSchema; source: string }>(res)
}

interface RawProposal {
  id: string
  title: string
  branch?: string
  source_branch?: string
  sourceBranch?: string
  author?: string | null
  tenant?: string | null
  status?: "open" | "merged" | "closed"
  state?: "open" | "merged" | "closed"
  createdAt?: string
  created_at?: string
  prUrl?: string
  url?: string
  previewStatus?: "pending" | "ready" | "failed"
}

function normalizeProposal(raw: RawProposal): Proposal {
  return {
    id: raw.id,
    title: raw.title,
    branch: raw.branch ?? raw.source_branch ?? raw.sourceBranch ?? "",
    author: raw.author || "keel",
    tenant: raw.tenant || "default",
    status: raw.status ?? raw.state ?? "open",
    createdAt: raw.createdAt ?? raw.created_at ?? "just now",
    prUrl: raw.prUrl ?? raw.url,
    previewStatus: raw.previewStatus ?? "ready",
  }
}

export async function getProposals(
  tenant?: string,
  status?: "open" | "merged" | "closed" | "all"
): Promise<Proposal[]> {
  const params = new URLSearchParams()
  if (tenant) params.set("tenant", tenant)
  if (status) params.set("status", status)
  const qs = params.toString()
  const url = qs ? `/api/v1/proposals?${qs}` : "/api/v1/proposals"
  const res = await fetch(url, {
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    RawProposal[] | { proposals?: RawProposal[] }
  >(res)
  const list = Array.isArray(raw) ? raw : (raw.proposals ?? [])
  return list.map(normalizeProposal)
}

function unwrapProposalResponse(
  raw: RawProposal | { proposal: RawProposal; branch?: string }
): Proposal {
  if ("proposal" in raw) {
    return normalizeProposal({
      ...raw.proposal,
      branch: raw.proposal.branch ?? raw.branch,
    })
  }
  return normalizeProposal(raw)
}

export async function createProposal(
  payload: CreateProposalPayload
): Promise<Proposal> {
  const res = await fetch("/api/v1/proposals", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify(payload),
  })
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export async function getProposalPreview(
  id: string | number
): Promise<PreviewReport> {
  const res = await fetch(
    `/api/v1/proposals/${encodeURIComponent(id)}/preview`,
    {
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<
    PreviewReport | { url?: string; report?: PreviewReport; gitDiff?: string }
  >(res)
  if ("report" in raw && raw.report) {
    return {
      prUrl: raw.url,
      policyStatus: raw.report.policyStatus ?? "passed",
      summary: raw.report.summary,
      resources: raw.report.resources ?? [],
      gitDiff: raw.gitDiff,
    }
  }
  return raw as PreviewReport
}

export async function mergeProposal(
  id: string | number
): Promise<{ success: boolean; message?: string }> {
  const res = await fetch(`/api/v1/proposals/${encodeURIComponent(id)}/merge`, {
    method: "POST",
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    | { success: boolean; message?: string }
    | { proposal?: { id?: string; state?: string } }
  >(res)
  if ("success" in raw) {
    return raw
  }
  const merged = raw.proposal?.state === "merged"
  return {
    success: merged,
    message: merged
      ? `PR #${raw.proposal?.id ?? id} merged cleanly!`
      : "Failed to merge PR proposal",
  }
}

export async function rejectProposal(
  id: string | number
): Promise<{ success: boolean; message?: string }> {
  const res = await fetch(
    `/api/v1/proposals/${encodeURIComponent(id)}/reject`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<
    | { success: boolean; message?: string }
    | { proposal?: { id?: string; state?: string } }
  >(res)
  if ("success" in raw) {
    return raw
  }
  const closed = raw.proposal?.state === "closed"
  return {
    success: closed,
    message: closed
      ? `PR #${raw.proposal?.id ?? id} rejected and closed.`
      : "Failed to reject PR proposal",
  }
}

export async function reopenProposal(
  id: string | number
): Promise<{ success: boolean; message?: string }> {
  const res = await fetch(
    `/api/v1/proposals/${encodeURIComponent(id)}/reopen`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<
    | { success: boolean; message?: string }
    | { proposal?: { id?: string; state?: string } }
  >(res)
  if ("success" in raw) {
    return raw
  }
  const reopened = raw.proposal?.state === "open"
  return {
    success: reopened,
    message: reopened
      ? `PR #${raw.proposal?.id ?? id} reopened.`
      : "Failed to reopen PR proposal",
  }
}

export interface StacksWithMeta {
  stacks: StackSummary[]
  gitRepoUrl?: string
  forgeProvider?: string
  stateBackend?: string
}

export async function getStacksWithMeta(): Promise<StacksWithMeta> {
  const res = await fetch("/api/v1/stacks", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    | StackSummary[]
    | {
        stacks?: StackSummary[]
        gitRepoUrl?: string
        forgeProvider?: string
        stateBackend?: string
      }
  >(res)
  if (Array.isArray(raw)) {
    return { stacks: raw }
  }
  return {
    stacks: raw.stacks ?? [],
    gitRepoUrl: raw.gitRepoUrl,
    forgeProvider: raw.forgeProvider,
    stateBackend: raw.stateBackend,
  }
}

export async function getStacks(): Promise<StackSummary[]> {
  const { stacks } = await getStacksWithMeta()
  return stacks
}

export async function getStackDiagnostics(
  namespace: string,
  name: string
): Promise<StackDiagnostics> {
  const res = await fetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/diagnostics`,
    { headers: getHeaders() }
  )
  return handleResponse<StackDiagnostics>(res)
}

export async function getReleaseDiagnostics(
  namespace: string,
  name: string
): Promise<StackDiagnostics> {
  const res = await fetch(
    `/api/v1/releases/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/diagnostics`,
    { headers: getHeaders() }
  )
  return handleResponse<StackDiagnostics>(res)
}

export async function getStackConfig(
  namespace: string,
  name: string
): Promise<ReleaseConfig> {
  const res = await fetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/config`,
    {
      headers: getHeaders(),
    }
  )
  return handleResponse<ReleaseConfig>(res)
}

export async function getReleaseConfig(
  namespace: string,
  name: string
): Promise<ReleaseConfig> {
  const res = await fetch(
    `/api/v1/releases/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/config`,
    {
      headers: getHeaders(),
    }
  )
  return handleResponse<ReleaseConfig>(res)
}

export async function deleteStack(
  namespace: string,
  name: string
): Promise<{
  id: string
  namespace: string
  branch: string
  proposal: Proposal
}> {
  const res = await fetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
    {
      method: "DELETE",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<{
    id: string
    namespace: string
    branch: string
    proposal: RawProposal
  }>(res)
  return {
    id: raw.id,
    namespace: raw.namespace,
    branch: raw.branch,
    proposal: normalizeProposal(raw.proposal),
  }
}

export async function deleteRelease(
  namespace: string,
  name: string
): Promise<{
  id: string
  namespace: string
  branch: string
  proposal: Proposal
}> {
  const res = await fetch(
    `/api/v1/releases/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
    {
      method: "DELETE",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<{
    id: string
    namespace: string
    branch: string
    proposal: RawProposal
  }>(res)
  return {
    id: raw.id,
    namespace: raw.namespace,
    branch: raw.branch,
    proposal: normalizeProposal(raw.proposal),
  }
}

export async function getBundleConfig(
  namespace: string,
  name: string
): Promise<{ tenant: string; name: string; values: Record<string, unknown> }> {
  const res = await fetch(
    `/api/v1/bundles/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/config`,
    {
      headers: getHeaders(),
    }
  )
  return handleResponse<{
    tenant: string
    name: string
    values: Record<string, unknown>
  }>(res)
}

export async function restartStackWorkspace(
  namespace: string,
  name: string
): Promise<RestartStackResponse> {
  const res = await fetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/restart`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  return handleResponse<RestartStackResponse>(res)
}

export async function reconcileRelease(
  namespace: string,
  name: string
): Promise<{
  reconciled: boolean
  namespace: string
  name: string
  message: string
}> {
  const res = await fetch(
    `/api/v1/releases/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/reconcile`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  return handleResponse<{
    reconciled: boolean
    namespace: string
    name: string
    message: string
  }>(res)
}

export async function getReleases(): Promise<ReleaseSummary[]> {
  const res = await fetch("/api/v1/releases", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    ReleaseSummary[] | { releases?: ReleaseSummary[] }
  >(res)
  return Array.isArray(raw) ? raw : (raw.releases ?? [])
}

export async function getTenants(): Promise<TenantSummary[]> {
  const res = await fetch("/api/v1/tenants", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    TenantSummary[] | { tenants?: TenantSummary[] }
  >(res)
  return Array.isArray(raw) ? raw : (raw.tenants ?? [])
}

export async function getProviders(): Promise<ProviderRegistryEntry[]> {
  const res = await fetch("/api/v1/providers", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    ProviderRegistryEntry[] | { providers?: ProviderRegistryEntry[] }
  >(res)
  return Array.isArray(raw) ? raw : (raw.providers ?? [])
}

export async function toggleProvider(
  id: string,
  enabled: boolean
): Promise<Proposal> {
  const res = await fetch(`/api/v1/providers/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: getHeaders(),
    body: JSON.stringify({ enabled }),
  })
  const raw = await handleResponse<Proposal | { proposal: Proposal }>(res)
  return "proposal" in raw ? raw.proposal : raw
}

export async function createTenant(id: string): Promise<Proposal> {
  const res = await fetch("/api/v1/tenants", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify({ id }),
  })
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export async function deleteTenant(id: string): Promise<Proposal> {
  const res = await fetch(`/api/v1/tenants/${encodeURIComponent(id)}`, {
    method: "DELETE",
    headers: getHeaders(),
  })
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export async function createCatalogApp(
  payload: CreateCatalogItemPayload
): Promise<Proposal> {
  const res = await fetch("/api/v1/catalog", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify(payload),
  })
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export async function updateCatalogItem(
  kind: "app" | "infra" | "bundle" | string,
  id: string,
  payload: Omit<CreateCatalogItemPayload, "kind" | "id">
): Promise<Proposal> {
  const normalizedKind =
    kind === "infrastructure" || kind === "infra"
      ? "infra"
      : kind === "bundle"
        ? "bundle"
        : "app"
  const res = await fetch(
    `/api/v1/catalog/${normalizedKind}/${encodeURIComponent(id)}`,
    {
      method: "PUT",
      headers: getHeaders(),
      body: JSON.stringify(payload),
    }
  )
  const raw = await handleResponse<
    RawProposal | { proposal: RawProposal; branch?: string }
  >(res)
  return unwrapProposalResponse(raw)
}

export function createEventSourceStream(
  onEvent: (event: CrdWatchEvent) => void,
  onError?: (err: Event) => void
): EventSource {
  const es = new EventSource("/api/v1/events/stream")
  const handleFrame = (e: MessageEvent) => {
    try {
      const parsed = JSON.parse(e.data) as CrdWatchEvent
      onEvent(parsed)
    } catch {
      // Ignore invalid JSON events
    }
  }
  if (typeof es.addEventListener === "function") {
    es.addEventListener("crd-watch", handleFrame as EventListener)
  }
  es.onmessage = handleFrame
  if (onError) {
    es.onerror = onError
  }
  return es
}
