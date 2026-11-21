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

export interface HashRoute {
  tab: ActiveTab
  /** `#preview/<id>`: selected PR/proposal id. */
  proposalId?: string
  /** `#catalog/<kind>`: catalog kind pre-filter. */
  kind?: CatalogKindFilter
}

export function parseHashRoute(hash: string): HashRoute {
  const [rawTab, rawParam] = hash
    .replace(/^#\/?/, "")
    .trim()
    .split("/", 2)
    .map((p) => decodeURIComponentSafe(p ?? ""))
  const tab = (VALID_TABS as readonly string[]).includes(rawTab)
    ? (rawTab as ActiveTab)
    : "topology"
  const param = rawParam?.trim()
  if (tab === "preview" && param && /^[A-Za-z0-9._-]{1,64}$/.test(param)) {
    return { tab, proposalId: param }
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
  if (route.tab === "preview" && route.proposalId) {
    return `#preview/${encodeURIComponent(route.proposalId)}`
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
