import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"
import { stringify as stringifyYaml } from "yaml"

/**
 * Demo mode (`?demo=true`) renders bundled fixture data without any live API calls.
 * It is available in development builds, and in production builds only when compiled with
 * `VITE_ENABLE_DEMO=true`, so a production control plane can never be made to show
 * fabricated state by a crafted link (UI-9). When active, the shell shows a persistent
 * "DEMO DATA" banner.
 */
export function isDemoModeAllowed(): boolean {
  const env = import.meta.env ?? {}
  return Boolean(env.DEV) || env.VITE_ENABLE_DEMO === "true"
}

export function isDemoMode(): boolean {
  return (
    typeof window !== "undefined" &&
    isDemoModeAllowed() &&
    new URLSearchParams(window.location.search).get("demo") === "true"
  )
}

export function formatForgeProvider(provider?: string): string {
  switch (provider?.toLowerCase()) {
    case "forgejo":
      return "Forgejo"
    case "gitea":
      return "Gitea"
    case "github":
      return "GitHub"
    case "gitlab":
      return "GitLab"
    case "local":
    case undefined:
    case "":
      return "Local Git"
    default:
      return provider!.charAt(0).toUpperCase() + provider!.slice(1)
  }
}

const NON_SSH_PROVIDERS = new Set([
  "kind",
  "kubernetes",
  "k8s",
  "helm",
  "manifest",
  "kustomize",
  "flux",
  "pulumi-k8s",
  "docker",
  "local",
  "cloudflare",
  "kubeconfig",
])

export function classifyStackResourceKind(stack: {
  name?: string
  namespace?: string
  provider?: string
  type?: string
  resourceKind?: string
  catalogId?: string
}): "vm" | "cluster" | "service" {
  if (
    stack.resourceKind === "vm" ||
    stack.resourceKind === "cluster" ||
    stack.resourceKind === "service"
  ) {
    return stack.resourceKind
  }

  const name = (stack.name || "").toLowerCase()
  const provider = (stack.provider || "").toLowerCase()
  const catalogId = (stack.catalogId || "").toLowerCase()

  if (
    name === "management-cluster-core" ||
    stack.namespace === "flux-system" ||
    stack.type === "management" ||
    provider === "kind" ||
    catalogId.includes("cluster") ||
    catalogId.includes("eks") ||
    catalogId.includes("gke") ||
    catalogId.includes("aks") ||
    catalogId.includes("k3s") ||
    name.includes("cluster") ||
    name.includes("eks") ||
    name.includes("gke") ||
    name.includes("aks") ||
    name.includes("k3s")
  ) {
    return "cluster"
  }

  if (
    provider === "kubernetes" ||
    provider === "k8s" ||
    provider === "helm" ||
    provider === "manifest" ||
    provider === "kustomize" ||
    provider === "flux" ||
    provider === "pulumi-k8s" ||
    provider === "cloudflare" ||
    name === "postgres-managed" ||
    name === "canary-gateway-config" ||
    name.includes("postgres") ||
    name.includes("db") ||
    name.includes("gateway") ||
    name.endsWith("-net") ||
    name === "net"
  ) {
    return "service"
  }

  return "vm"
}

export function isStackSshSupported(stack: {
  name?: string
  namespace?: string
  provider?: string
  type?: string
  resourceKind?: string
  catalogId?: string
}): boolean {
  const provider = (stack.provider || "").trim().toLowerCase()
  if (NON_SSH_PROVIDERS.has(provider)) {
    return false
  }
  return classifyStackResourceKind(stack) === "vm"
}

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/**
 * Serializes JSON-compatible data to YAML using the `yaml` library so strings that would
 * otherwise be re-typed (`"true"`, `"123"`, `""`, `"null"`) or alter structure (leading `-`,
 * `: `, `#`, newlines) are quoted/escaped correctly.
 */
export function jsonToYaml(obj: unknown): string {
  if (
    obj === undefined ||
    typeof obj === "function" ||
    typeof obj === "symbol"
  ) {
    return "null\n"
  }
  return stringifyYaml(obj, { lineWidth: 0 })
}
