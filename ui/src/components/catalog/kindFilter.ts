import type { CatalogKindFilter } from "@/lib/routes"
import { CatalogItem } from "@/types/api"

export const KIND_FILTER_LABELS: Record<CatalogKindFilter, string> = {
  cluster: "Kubernetes clusters",
  vm: "Virtual machines",
  service: "Managed services",
  app: "Applications",
}

/**
 * Whether a catalog item belongs to the "+ Deploy <kind>" pre-filter. Infra blueprints use the
 * server-resolved `resourceKind`; apps match `app`. Bundles provision
 * clusters + apps, so they are listed under `cluster` and `app`.
 */
export function matchesKindFilter(
  item: CatalogItem,
  kind: CatalogKindFilter | undefined
): boolean {
  if (!kind) return true
  if (item.type === "bundle") return kind === "cluster" || kind === "app"
  if (item.type === "app") return kind === "app"
  return item.type === "infra" && item.resourceKind === kind
}
