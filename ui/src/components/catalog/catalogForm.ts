import type { RJSFSchema } from "@rjsf/utils"

import { DEFAULT_STARTER_MANIFEST } from "@/lib/starterManifest"
import {
  CatalogBundleItem,
  CatalogItem,
  CreateCatalogItemPayload,
} from "@/types/api"

export function emptyCatalogForm(): CreateCatalogItemPayload {
  return {
    kind: "app",
    id: "",
    name: "",
    category: "Applications",
    description: "",
    runtime: "helm",
    repoUrl: "",
    chart: "",
    version: "",
    path: "",
    flaggerSupported: false,
    provider: "kind",
    pulumiRuntime: "yaml",
    entrypoint: "Pulumi.yaml",
    protectByDefault: false,
    terraformProvider: "",
    pulumiYaml: "",
    manifestsYaml: "",
    defaultEngine: "flux",
    items: [],
  }
}

/** Populates the catalog editor form from an existing catalog item. */
export function catalogItemToForm(item: CatalogItem): CreateCatalogItemPayload {
  const kind = item.type
  return {
    kind,
    id: item.id,
    name: item.name,
    category:
      item.category ||
      (kind === "infra"
        ? "Infrastructure"
        : kind === "bundle"
          ? "Platform"
          : "Applications"),
    description: item.description || "",
    runtime: (item.runtime as "helm" | "kustomize" | "manifest") || "helm",
    manifestsYaml:
      item.manifestsYaml ||
      (item.runtime === "manifest" ? DEFAULT_STARTER_MANIFEST : ""),
    defaultEngine:
      item.defaultEngine || (item.runtime === "manifest" ? "flux" : undefined),
    placementScope: item.placementScope,
    repoUrl: item.repoUrl || "",
    chart: item.chart || "",
    version: item.version || "",
    path: item.path || "",
    flaggerSupported: !!item.flaggerSupported,
    provider: item.provider || "kind",
    pulumiRuntime: item.pulumiRuntime || "yaml",
    entrypoint: item.entrypoint || "Pulumi.yaml",
    protectByDefault: !!item.protectByDefault,
    protected: Boolean(item.protected),
    tenant: item.tenant || "",
    resourceKind: item.resourceKind,
    terraformProvider: "",
    pulumiYaml: item.pulumiYaml || "",
    items: item.bundleItems
      ? item.bundleItems.map((b) => ({
          ...b,
          dependsOn: b.dependsOn ? [...b.dependsOn] : [],
        }))
      : [],
  }
}

/** Mirrors the submit-button enablement rules of the catalog editor. */
export function isCatalogFormSubmittable(
  form: CreateCatalogItemPayload
): boolean {
  const kind = form.kind || "app"
  if (!form.id.trim() || !form.name.trim()) return false
  if (kind === "app" && form.runtime !== "manifest" && !form.repoUrl?.trim()) {
    return false
  }
  if (kind === "bundle") {
    const items = form.items ?? []
    if (
      items.length === 0 ||
      items.some((it) => !it.id.trim() || !it.catalogRef.trim())
    ) {
      return false
    }
  }
  return true
}

const optionalOrEmpty = (isEditing: boolean, v?: string) =>
  isEditing ? (v?.trim() ?? "") : v?.trim() || undefined

/** Builds the create (`POST /api/v1/catalog`) payload from the editor form. */
export function buildCatalogPayload(
  form: CreateCatalogItemPayload,
  editing: CatalogItem | null,
  schema?: RJSFSchema
): CreateCatalogItemPayload {
  const isEditing = Boolean(editing)
  const kind = form.kind || "app"
  const protectedChanged =
    isEditing && Boolean(form.protected) !== Boolean(editing?.protected)
  const tenantChanged =
    isEditing && (form.tenant?.trim() || "") !== (editing?.tenant || "")
  const placementChanged =
    isEditing &&
    (form.placementScope ?? "namespaced") !==
      (editing?.placementScope ?? "namespaced")
  const base = {
    id: form.id.trim(),
    name: form.name.trim(),
    ...(form.protected || protectedChanged
      ? { protected: Boolean(form.protected) }
      : {}),
    ...(form.tenant?.trim()
      ? { tenant: form.tenant.trim() }
      : tenantChanged
        ? { tenant: "" }
        : {}),
    ...(schema ? { schema } : {}),
  }
  const description = isEditing
    ? { description: form.description?.trim() ?? "" }
    : form.description?.trim()
      ? { description: form.description.trim() }
      : {}

  if (kind === "bundle") {
    return {
      kind: "bundle",
      ...base,
      category: form.category.trim() || "Platform",
      ...description,
      items: (form.items ?? []).map((item) => ({
        id: item.id.trim(),
        kind: item.kind,
        catalogRef: item.catalogRef.trim(),
        ...(item.dependsOn && item.dependsOn.length > 0
          ? { dependsOn: item.dependsOn }
          : {}),
        ...(item.kind === "app" && item.targetClusterFrom?.trim()
          ? { targetClusterFrom: item.targetClusterFrom.trim() }
          : {}),
        ...(item.kind === "app" && item.targetNamespace?.trim()
          ? { targetNamespace: item.targetNamespace.trim() }
          : {}),
        ...(item.kind === "app" && item.canary ? { canary: true } : {}),
      })),
    }
  }
  if (kind === "infra") {
    return {
      kind: "infra",
      ...base,
      category: form.category.trim() || "Infrastructure",
      ...(form.resourceKind ? { resourceKind: form.resourceKind } : {}),
      ...description,
      provider: form.provider || "kind",
      pulumiRuntime: form.pulumiRuntime || "yaml",
      entrypoint: form.entrypoint?.trim() || "Pulumi.yaml",
      terraformProvider: form.terraformProvider?.trim() || undefined,
      protectByDefault: !!form.protectByDefault,
      ...(form.pulumiYaml?.trim() ? { pulumiYaml: form.pulumiYaml } : {}),
    }
  }
  const runtime = form.runtime || "helm"
  const source =
    runtime === "manifest"
      ? {
          manifestsYaml: form.manifestsYaml || DEFAULT_STARTER_MANIFEST,
          defaultEngine: form.defaultEngine || "flux",
          repoUrl: !form.repoUrl?.trim()
            ? "https://kubernetes.io/docs/concepts/overview/working-with-objects/"
            : form.repoUrl.trim(),
          path: !form.path?.trim() ? "./manifests.yaml" : form.path.trim(),
          chart: isEditing ? "" : undefined,
          version: isEditing ? "" : undefined,
        }
      : runtime === "kustomize"
        ? {
            repoUrl: form.repoUrl?.trim() || "",
            path: optionalOrEmpty(isEditing, form.path),
            version: optionalOrEmpty(isEditing, form.version),
            chart: isEditing ? "" : undefined,
          }
        : {
            repoUrl: form.repoUrl?.trim() || "",
            chart: optionalOrEmpty(isEditing, form.chart),
            version: optionalOrEmpty(isEditing, form.version),
            path: isEditing ? "" : undefined,
          }
  return {
    kind: "app",
    ...base,
    category: form.category.trim() || "Applications",
    ...description,
    runtime,
    ...source,
    flaggerSupported: !!form.flaggerSupported,
    // Sent when cluster-scoped, or on edit when it changed (so it can be reset to namespaced).
    ...(form.placementScope === "cluster" || placementChanged
      ? { placementScope: form.placementScope ?? "namespaced" }
      : {}),
  }
}

/** Projects a full payload onto the `PUT /api/v1/catalog/{kind}/{id}` body. */
export function toUpdatePayload(
  payload: CreateCatalogItemPayload
): Omit<CreateCatalogItemPayload, "kind" | "id"> {
  return {
    name: payload.name,
    category: payload.category,
    description: payload.description,
    runtime: payload.runtime,
    manifestsYaml: payload.manifestsYaml,
    defaultEngine: payload.defaultEngine,
    repoUrl: payload.repoUrl,
    chart: payload.chart,
    version: payload.version,
    path: payload.path,
    flaggerSupported: payload.flaggerSupported,
    provider: payload.provider,
    pulumiRuntime: payload.pulumiRuntime,
    entrypoint: payload.entrypoint,
    protectByDefault: payload.protectByDefault,
    ...(payload.protected !== undefined
      ? { protected: payload.protected }
      : {}),
    ...(payload.tenant !== undefined ? { tenant: payload.tenant } : {}),
    ...(payload.resourceKind ? { resourceKind: payload.resourceKind } : {}),
    ...(payload.placementScope
      ? { placementScope: payload.placementScope }
      : {}),
    ...(payload.pulumiYaml ? { pulumiYaml: payload.pulumiYaml } : {}),
    items: payload.items,
    schema: payload.schema,
  }
}

export const isInfraItem = (c: CatalogItem) => c.type === "infra"

/** Default first step when switching a new catalog item to the bundle kind. */
export function defaultBundleStep(catalog: CatalogItem[]): CatalogBundleItem {
  const hasInfra = catalog.some(isInfraItem)
  return {
    id: "step-1",
    kind: hasInfra ? "infra" : "app",
    catalogRef:
      catalog.find(isInfraItem)?.id ??
      catalog.find((c) => c.type === "app")?.id ??
      "",
    dependsOn: [],
  }
}

/** Builds a starter `Pulumi.yaml` (optionally bridging an OpenTofu/Terraform provider). */
export function starterPulumiYaml(form: {
  id: string
  name: string
  pulumiRuntime?: string
  provider?: string
  terraformProvider?: string
}): string {
  const bpId = form.id.trim() || "my-blueprint"
  const bpName = form.name.trim() || bpId
  const runtime = form.pulumiRuntime || "yaml"
  const provider = form.provider || "kind"
  const tf = form.terraformProvider?.trim()
  const pkgBlock = tf
    ? `packages:\n  ${provider}:\n    source: terraform-provider\n    parameters:\n      - ${tf}\n`
    : ""
  return `name: ${bpId}\nruntime: ${runtime}\ndescription: ${bpName} (${provider})\n${pkgBlock}config:\n  name:\n    type: string\n    default: workload-1\nresources: {}\noutputs: {}\n`
}
