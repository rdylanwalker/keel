import { RJSFSchema } from "@rjsf/utils"
import {
  AuthStatus,
  CatalogBundleItem,
  CatalogItem,
  CatalogKind,
  CreateCatalogItemPayload,
  CreateProposalPayload,
  CreateTenantResult,
  CrdWatchEvent,
  DeleteTenantResult,
  ProposalListing,
  ProposalRef,
  ProposalRepository,
  TenantRepositoryStatus,
  UnavailableRepository,
  PreviewReport,
  RawResourceDiff,
  ResourceDiff,
  Proposal,
  SecretNotice,
  ProviderRegistryEntry,
  ReleaseConfig,
  ReleaseSummary,
  RestartStackResponse,
  ForceDeleteStackResponse,
  UnlockStackResponse,
  StackDiagnostics,
  StackSshInfo,
  StackSummary,
  TenantSummary,
  UpgradeManagementAddonPayload,
} from "../types/api"
import { isDemoMode } from "./utils"
import { repositoryWebUrl, safeHttpUrl } from "./forgeLinks"
import { DemoModeError, parseApiError } from "./errors"

/** Request headers; authentication rides on the HttpOnly session cookie. */
function getHeaders(): Record<string, string> {
  return { "Content-Type": "application/json" }
}

export { DemoModeError }

/**
 * Single network entry point for the Keel REST API. In demo mode no request ever leaves the
 * browser: callers receive a {@link DemoModeError} and keep rendering bundled fixtures (UI-9).
 */
async function apiFetch(input: string, init?: RequestInit): Promise<Response> {
  if (isDemoMode()) {
    throw new DemoModeError()
  }
  return fetch(input, init)
}

async function handleResponse<T>(res: Response): Promise<T> {
  if (!res.ok) {
    if (res.status === 401 && typeof window !== "undefined") {
      window.dispatchEvent(new CustomEvent("keel:unauthorized"))
    }
    let body: string | undefined
    try {
      if (typeof res.text === "function") {
        body = await res.text()
      }
    } catch {
      // fallback to statusText
    }
    throw parseApiError(res.status, body, res.statusText)
  }
  return res.json() as Promise<T>
}

const POST_LOGIN_HASH_KEY = "keel:post-login-hash"

/** Remembers the current deep link (hash route only; never secrets) before an OIDC redirect. */
export function rememberPostLoginLocation(): void {
  try {
    const hash = window.location.hash
    if (hash && hash !== "#") {
      window.sessionStorage.setItem(POST_LOGIN_HASH_KEY, hash)
    }
  } catch {
    // sessionStorage unavailable (privacy mode); deep link is best-effort.
  }
}

/** Returns and clears the deep link stored by {@link rememberPostLoginLocation}. */
export function consumePostLoginLocation(): string | null {
  try {
    const hash = window.sessionStorage.getItem(POST_LOGIN_HASH_KEY)
    window.sessionStorage.removeItem(POST_LOGIN_HASH_KEY)
    return hash && /^#[A-Za-z0-9/_.-]*$/.test(hash) ? hash : null
  } catch {
    return null
  }
}

export function startOidcLogin(): void {
  rememberPostLoginLocation()
  window.location.assign("/api/v1/auth/login")
}

export async function getAuthStatus(): Promise<AuthStatus> {
  const res = await apiFetch("/api/v1/auth/status", {
    headers: getHeaders(),
  })
  return handleResponse<AuthStatus>(res)
}

export async function logout(): Promise<{ authenticated: boolean }> {
  const res = await apiFetch("/api/v1/auth/logout", {
    method: "POST",
    headers: getHeaders(),
  })
  return handleResponse<{ authenticated: boolean }>(res)
}

interface RawCatalogEntry {
  metadata?: {
    id?: string
    name?: string
    description?: string
    category?: string
    runtime?: string
    provider?: string
    defaultEngine?: "flux" | "pulumi"
    protected?: boolean
    tenant?: string
    resourceKind?: "vm" | "cluster" | "service"
  }
  spec?: {
    upstream?: {
      repoURL?: string
      chart?: string
      version?: string
      path?: string
    }
    manifestsYaml?: string
    defaultEngine?: "flux" | "pulumi"
    placement?: {
      scope?: string
      namespace?: string
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
    manifestsYaml: app.spec?.manifestsYaml,
    defaultEngine: app.spec?.defaultEngine ?? app.metadata?.defaultEngine,
    ...(app.spec?.placement?.scope === "cluster"
      ? { placementScope: "cluster" as const }
      : {}),
    ...(app.spec?.placement?.scope === "cluster" && app.spec.placement.namespace
      ? { placementNamespace: app.spec.placement.namespace }
      : {}),
    flaggerSupported: app.spec?.progressiveDelivery?.flaggerSupported ?? false,
    defaultCanary: app.spec?.progressiveDelivery?.defaultCanary ?? false,
    serviceUrl: app.spec?.serviceUrl,
    schema: app.spec?.schema ?? { type: "object", properties: {} },
    uiSchema: app.spec?.uiSchema,
    protected: Boolean(app.metadata?.protected),
    tenant: app.metadata?.tenant,
  }
}

export async function getCatalog(): Promise<CatalogItem[]> {
  const res = await apiFetch("/api/v1/catalog", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<RawCatalogResponse>(res)
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
    protected: Boolean(bp.metadata?.protected),
    tenant: bp.metadata?.tenant,
    resourceKind: bp.metadata?.resourceKind,
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
    protected: Boolean(b.metadata?.protected),
    tenant: b.metadata?.tenant,
  }))
  return [...bundles, ...blueprints, ...apps]
}

export async function getCoreAddons(): Promise<CatalogItem[]> {
  const res = await apiFetch("/api/v1/catalog", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<RawCatalogResponse>(res)
  return (raw.coreAddons ?? []).map(mapCatalogApplication)
}

export async function upgradeManagementAddon(
  id: string,
  payload: UpgradeManagementAddonPayload
): Promise<Proposal> {
  const res = await apiFetch(
    `/api/v1/management/addons/${encodeURIComponent(id)}/upgrade`,
    {
      method: "POST",
      headers: getHeaders(),
      body: JSON.stringify(payload),
    }
  )
  const raw = await handleResponse<{ proposal: RawProposal }>(res)
  return normalizeProposal(raw.proposal)
}

export async function rollbackManagementAddon(id: string): Promise<Proposal> {
  const res = await apiFetch(
    `/api/v1/management/addons/${encodeURIComponent(id)}/rollback`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<{ proposal: RawProposal }>(res)
  return normalizeProposal(raw.proposal)
}

export async function fetchCatalogSchema(payload: {
  repoUrl: string
  chart: string
  version?: string
}): Promise<{ schema: RJSFSchema; source: string }> {
  const res = await apiFetch("/api/v1/catalog/fetch-schema", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify(payload),
  })
  return handleResponse<{ schema: RJSFSchema; source: string }>(res)
}

/** Wire proposal (`ScopedProposal`: the forge `PullRequestInfo` plus scope and repository). */
interface RawProposal {
  id: string
  title: string
  sourceBranch: string
  tenant?: string | null
  state: "open" | "merged" | "closed"
  createdAt: string
  url: string
  scope: string
  repository: ProposalRepository
}

/**
 * Route scope of a wire proposal. The server always sends `scope`; guessing it from `tenant`
 * would address the wrong repository, so a missing scope is an error.
 */
function proposalScope(raw: RawProposal): string {
  if (!raw.scope) {
    throw new Error(`Proposal ${raw.id} has no scope in the server response`)
  }
  return raw.scope
}

function normalizeProposal(raw: RawProposal): Proposal {
  const scope = proposalScope(raw)
  const prUrl = raw.url
  const proposal: Proposal = {
    id: raw.id,
    title: raw.title,
    branch: raw.sourceBranch,
    tenant: raw.tenant || scope,
    status: raw.state,
    createdAt: raw.createdAt,
    prUrl,
    scope,
    repository: raw.repository,
  }
  const repoUrl = repositoryWebUrl(prUrl)
  if (repoUrl) proposal.repoUrl = repoUrl
  return proposal
}

/** Stable key of a proposal (PR ids are per repository). */
export function proposalKey(ref: ProposalRef): string {
  return `${ref.scope}/${ref.id}`
}

/** `/api/v1/tenants/{scope}/proposals/{id}` (+ optional action suffix). */
function proposalPath(ref: ProposalRef, suffix?: string): string {
  const base = `/api/v1/tenants/${encodeURIComponent(ref.scope)}/proposals/${encodeURIComponent(ref.id)}`
  return suffix ? `${base}/${suffix}` : base
}

/**
 * Lists proposals: one scope (`/api/v1/tenants/{tenant}/proposals`; `management` = platform
 * repository) or, without `tenant`, every repository the caller may read
 * (`/api/v1/proposals`, admins see all tenants). `unavailable` names repositories whose PRs are
 * missing or stale.
 */
export async function getProposalListing(
  tenant?: string,
  status?: "open" | "merged" | "closed" | "all"
): Promise<ProposalListing> {
  const params = new URLSearchParams()
  if (status) params.set("status", status)
  const qs = params.toString()
  const base = tenant
    ? `/api/v1/tenants/${encodeURIComponent(tenant)}/proposals`
    : "/api/v1/proposals"
  const res = await apiFetch(qs ? `${base}?${qs}` : base, {
    headers: getHeaders(),
  })
  const raw = await handleResponse<{
    proposals: RawProposal[]
    unavailable?: UnavailableRepository[]
  }>(res)
  return {
    proposals: raw.proposals.map(normalizeProposal),
    unavailable: raw.unavailable ?? [],
  }
}

export async function getProposals(
  tenant?: string,
  status?: "open" | "merged" | "closed" | "all"
): Promise<Proposal[]> {
  const { proposals } = await getProposalListing(tenant, status)
  return proposals
}

/** `POST /api/v1/tenants/{tenant}/proposals`: opens the PR in the tenant's repository. */
export async function createProposal(
  payload: CreateProposalPayload
): Promise<Proposal> {
  const res = await apiFetch(
    `/api/v1/tenants/${encodeURIComponent(payload.tenant)}/proposals`,
    {
      method: "POST",
      headers: getHeaders(),
      body: JSON.stringify(payload),
    }
  )
  const raw = await handleResponse<{
    proposal: RawProposal
    secretNotices?: SecretNotice[]
  }>(res)
  const proposal = normalizeProposal(raw.proposal)
  if (Array.isArray(raw.secretNotices) && raw.secretNotices.length > 0) {
    proposal.secretNotices = raw.secretNotices
  }
  return proposal
}

/** Maps a wire (camelCase) resource diff to the UI shape. */
export function normalizeResourceDiff(raw: RawResourceDiff): ResourceDiff {
  return {
    urn: raw.urn ?? "",
    name: raw.name ?? "",
    op: raw.op,
    resource_type: raw.resourceType ?? "",
    changed_fields: raw.changedFields ?? [],
  }
}

type RawPreviewReport = Omit<PreviewReport, "resources"> & {
  resources?: RawResourceDiff[]
}

export async function getProposalPreview(
  ref: ProposalRef
): Promise<PreviewReport> {
  const res = await apiFetch(proposalPath(ref, "preview"), {
    headers: getHeaders(),
  })
  const raw = await handleResponse<{
    url: string
    report: RawPreviewReport
    gitDiff?: string
  }>(res)
  return {
    prUrl: raw.url,
    // Never default to "passed": absent status means policy was not evaluated.
    policyStatus: raw.report.policyStatus,
    source: raw.report.source,
    run: raw.report.run,
    summary: raw.report.summary,
    resources: (raw.report.resources ?? []).map(normalizeResourceDiff),
    gitDiff: raw.gitDiff,
  }
}

export async function mergeProposal(
  ref: ProposalRef
): Promise<{ success: boolean; message?: string }> {
  const res = await apiFetch(proposalPath(ref, "merge"), {
    method: "POST",
    headers: getHeaders(),
  })
  const raw = await handleResponse<{ proposal: { id: string; state: string } }>(
    res
  )
  const merged = raw.proposal.state === "merged"
  return {
    success: merged,
    message: merged
      ? `PR #${raw.proposal.id} merged cleanly!`
      : "Failed to merge PR proposal",
  }
}

export async function rejectProposal(
  ref: ProposalRef
): Promise<{ success: boolean; message?: string }> {
  const res = await apiFetch(proposalPath(ref, "reject"), {
    method: "POST",
    headers: getHeaders(),
  })
  const raw = await handleResponse<{ proposal: { id: string; state: string } }>(
    res
  )
  const closed = raw.proposal.state === "closed"
  return {
    success: closed,
    message: closed
      ? `PR #${raw.proposal.id} rejected and closed.`
      : "Failed to reject PR proposal",
  }
}

export async function reopenProposal(
  ref: ProposalRef
): Promise<{ success: boolean; message?: string }> {
  const res = await apiFetch(proposalPath(ref, "reopen"), {
    method: "POST",
    headers: getHeaders(),
  })
  const raw = await handleResponse<{ proposal: { id: string; state: string } }>(
    res
  )
  const reopened = raw.proposal.state === "open"
  return {
    success: reopened,
    message: reopened
      ? `PR #${raw.proposal.id} reopened.`
      : "Failed to reopen PR proposal",
  }
}

export interface StacksWithMeta {
  stacks: StackSummary[]
  gitRepoUrl?: string
  forgeProvider?: string
  stateBackend?: string
}

export async function getStacksWithMeta(
  tenant?: string
): Promise<StacksWithMeta> {
  const qs = tenant ? `?tenant=${encodeURIComponent(tenant)}` : ""
  const res = await apiFetch(`/api/v1/stacks${qs}`, {
    headers: getHeaders(),
  })
  const raw = await handleResponse<StacksWithMeta>(res)
  return {
    stacks: raw.stacks,
    gitRepoUrl: raw.gitRepoUrl,
    forgeProvider: raw.forgeProvider,
    stateBackend: raw.stateBackend,
  }
}

export async function getStacks(tenant?: string): Promise<StackSummary[]> {
  const { stacks } = await getStacksWithMeta(tenant)
  return stacks
}

export async function getStackDiagnostics(
  namespace: string,
  name: string
): Promise<StackDiagnostics> {
  const res = await apiFetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/diagnostics`,
    { headers: getHeaders() }
  )
  return handleResponse<StackDiagnostics>(res)
}

export async function fetchStackSshInfo(
  namespace: string,
  name: string
): Promise<StackSshInfo> {
  if (isDemoMode()) {
    const tenant = namespace.startsWith("tenant-")
      ? namespace.replace("tenant-", "")
      : namespace
    return {
      stackName: name,
      namespace,
      tenant,
      provider: "proxmox",
      sshHost: "192.168.1.150",
      sshUser: "debian",
      sshPort: 22,
      sshCommand: `ssh debian@192.168.1.150 -p 22`,
      cliCommand: `keel infra ssh ${name} --tenant ${tenant}`,
      cliExecCommand: `keel infra ssh ${name} --tenant ${tenant} --exec`,
      hasSavedSshKey: true,
    }
  }

  const res = await apiFetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/ssh-info`,
    { headers: getHeaders() }
  )
  return handleResponse<StackSshInfo>(res)
}

export async function getReleaseDiagnostics(
  namespace: string,
  name: string
): Promise<StackDiagnostics> {
  const res = await apiFetch(
    `/api/v1/releases/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/diagnostics`,
    { headers: getHeaders() }
  )
  return handleResponse<StackDiagnostics>(res)
}

export async function getStackConfig(
  namespace: string,
  name: string
): Promise<ReleaseConfig> {
  const res = await apiFetch(
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
  const res = await apiFetch(
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
  const res = await apiFetch(
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
  const res = await apiFetch(
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

export async function restartStackWorkspace(
  namespace: string,
  name: string
): Promise<RestartStackResponse> {
  const res = await apiFetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/restart`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  return handleResponse<RestartStackResponse>(res)
}

export async function reconcileStack(
  namespace: string,
  name: string
): Promise<{
  reconciled: boolean
  namespace: string
  name: string
  message: string
}> {
  const res = await apiFetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/reconcile`,
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

export async function forceDeleteStack(
  namespace: string,
  name: string
): Promise<ForceDeleteStackResponse> {
  const res = await apiFetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/force-delete`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  return handleResponse<ForceDeleteStackResponse>(res)
}

export async function unlockStack(
  namespace: string,
  name: string
): Promise<UnlockStackResponse> {
  const res = await apiFetch(
    `/api/v1/stacks/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/unlock`,
    {
      method: "POST",
      headers: getHeaders(),
    }
  )
  return handleResponse<UnlockStackResponse>(res)
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
  const res = await apiFetch(
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

export async function getReleases(tenant?: string): Promise<ReleaseSummary[]> {
  const qs = tenant ? `?tenant=${encodeURIComponent(tenant)}` : ""
  const res = await apiFetch(`/api/v1/releases${qs}`, {
    headers: getHeaders(),
  })
  const raw = await handleResponse<{ releases: ReleaseSummary[] }>(res)
  return raw.releases
}

export async function getTenants(): Promise<TenantSummary[]> {
  const res = await apiFetch("/api/v1/tenants", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<{ tenants: TenantSummary[] }>(res)
  return raw.tenants
}

export async function getProviders(): Promise<ProviderRegistryEntry[]> {
  const res = await apiFetch("/api/v1/providers", {
    headers: getHeaders(),
  })
  const raw = await handleResponse<{ providers: ProviderRegistryEntry[] }>(res)
  return raw.providers
}

export async function toggleProvider(
  id: string,
  enabled: boolean
): Promise<Proposal> {
  const res = await apiFetch(`/api/v1/providers/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: getHeaders(),
    body: JSON.stringify({ enabled }),
  })
  const raw = await handleResponse<{ proposal: RawProposal }>(res)
  return normalizeProposal(raw.proposal)
}

interface RawCreateTenantResponse {
  tenant: { id: string; namespace: string }
  proposal?: RawProposal | null
  branch?: string | null
  repository: Omit<TenantRepositoryStatus, "skeletonProposal" | "notes"> & {
    skeletonProposal?: RawProposal | null
    notes?: string[]
  }
}

/**
 * `POST /api/v1/tenants`: provisions the tenant's repository + read credential (remote forges)
 * and opens the platform stub PR. `adoptExisting` adopts a repository that already holds content.
 */
export async function createTenant(
  id: string,
  options: { adoptExisting?: boolean } = {}
): Promise<CreateTenantResult> {
  const res = await apiFetch("/api/v1/tenants", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify(
      options.adoptExisting ? { id, adoptExisting: true } : { id }
    ),
  })
  const raw = await handleResponse<RawCreateTenantResponse>(res)
  const { skeletonProposal, notes, ...repo } = raw.repository
  const repository: TenantRepositoryStatus = {
    ...repo,
    notes: notes ?? [],
  }
  if (skeletonProposal) {
    repository.skeletonProposal = normalizeProposal(skeletonProposal)
  }
  if (repository.webUrl !== undefined) {
    repository.webUrl = safeHttpUrl(repository.webUrl)
  }
  return {
    tenant: raw.tenant,
    proposal: raw.proposal ? normalizeProposal(raw.proposal) : undefined,
    branch: raw.branch ?? undefined,
    repository,
  }
}

/** `DELETE /api/v1/tenants/{id}`: opens the PR removing the tenant's platform stub. */
export async function deleteTenant(id: string): Promise<DeleteTenantResult> {
  const res = await apiFetch(`/api/v1/tenants/${encodeURIComponent(id)}`, {
    method: "DELETE",
    headers: getHeaders(),
  })
  const raw = await handleResponse<{
    id: string
    proposal: RawProposal
    branch: string
    notes: string[]
  }>(res)
  return {
    id: raw.id,
    proposal: normalizeProposal(raw.proposal),
    branch: raw.branch,
    notes: raw.notes,
  }
}

export async function createCatalogApp(
  payload: CreateCatalogItemPayload
): Promise<Proposal> {
  const res = await apiFetch("/api/v1/catalog", {
    method: "POST",
    headers: getHeaders(),
    body: JSON.stringify(payload),
  })
  const raw = await handleResponse<{ proposal: RawProposal }>(res)
  return normalizeProposal(raw.proposal)
}

export async function updateCatalogItem(
  kind: CatalogKind,
  id: string,
  payload: Omit<CreateCatalogItemPayload, "kind" | "id">
): Promise<Proposal> {
  const res = await apiFetch(
    `/api/v1/catalog/${kind}/${encodeURIComponent(id)}`,
    {
      method: "PUT",
      headers: getHeaders(),
      body: JSON.stringify(payload),
    }
  )
  const raw = await handleResponse<{ proposal: RawProposal }>(res)
  return normalizeProposal(raw.proposal)
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
  es.addEventListener("crd-watch", handleFrame as EventListener)
  if (onError) {
    es.onerror = onError
  }
  return es
}

export async function deleteCatalogItem(
  kind: CatalogKind,
  id: string
): Promise<Proposal> {
  const res = await apiFetch(
    `/api/v1/catalog/${kind}/${encodeURIComponent(id)}`,
    {
      method: "DELETE",
      headers: getHeaders(),
    }
  )
  const raw = await handleResponse<{ proposal: RawProposal }>(res)
  return normalizeProposal(raw.proposal)
}

/** `GET /api/v1/management/addons` — admin-only live status of management core add-ons. */
export async function getManagementAddons(): Promise<
  import("../types/api").ManagementAddonsResponse
> {
  const res = await apiFetch("/api/v1/management/addons", {
    headers: getHeaders(),
  })
  const raw =
    await handleResponse<
      Partial<import("../types/api").ManagementAddonsResponse>
    >(res)
  return {
    addons: Array.isArray(raw.addons) ? raw.addons : [],
    liveStatusAvailable: Boolean(raw.liveStatusAvailable),
  }
}
