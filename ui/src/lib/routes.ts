/** Hash-route model for the single-page app (`#tab[/param]`). */
export type ActiveTab = "topology" | "catalog" | "preview" | "providers"

/** Catalog pre-filter used by "+ Deploy Cluster/VM/Service/App" shortcuts. */
export type CatalogKindFilter = "cluster" | "vm" | "service" | "app"

export const VALID_TABS: readonly ActiveTab[] = [
  "topology",
  "catalog",
  "preview",
  "providers",
]

const KIND_FILTERS: readonly CatalogKindFilter[] = [
  "cluster",
  "vm",
  "service",
  "app",
]

/** Proposal scope segment: a tenant id or `management` (DNS-1123 label). */
const DNS_LABEL = /^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$/
const PR_ID = /^[A-Za-z0-9._-]{1,64}$/

export interface HashRoute {
  tab: ActiveTab
  /** `#preview/<scope>/<id>`: selected PR id (unique within its repository only). */
  proposalId?: string
  /** `#preview/<scope>/<id>`: proposal scope (tenant id or `management`). */
  proposalScope?: string
  /** `#catalog/<kind>`: catalog kind pre-filter. */
  kind?: CatalogKindFilter
}

export function parseHashRoute(hash: string): HashRoute {
  const [rawTab, rawParam, rawParam2] = hash
    .replace(/^#\/?/, "")
    .trim()
    .split("/", 3)
    .map((p) => decodeURIComponentSafe(p ?? ""))
  const tab = (VALID_TABS as readonly string[]).includes(rawTab)
    ? (rawTab as ActiveTab)
    : "topology"
  const param = rawParam?.trim()
  const param2 = rawParam2?.trim()
  if (
    tab === "preview" &&
    param &&
    param2 &&
    DNS_LABEL.test(param) &&
    PR_ID.test(param2)
  ) {
    return { tab, proposalScope: param, proposalId: param2 }
  }
  if (
    tab === "catalog" &&
    param &&
    (KIND_FILTERS as readonly string[]).includes(param)
  ) {
    return { tab, kind: param as CatalogKindFilter }
  }
  return { tab }
}

export function formatHashRoute(route: HashRoute): string {
  if (route.tab === "preview" && route.proposalId && route.proposalScope) {
    return `#preview/${encodeURIComponent(route.proposalScope)}/${encodeURIComponent(route.proposalId)}`
  }
  if (route.tab === "catalog" && route.kind) {
    return `#catalog/${route.kind}`
  }
  return `#${route.tab}`
}

function decodeURIComponentSafe(v: string): string {
  try {
    return decodeURIComponent(v)
  } catch {
    return v
  }
}
