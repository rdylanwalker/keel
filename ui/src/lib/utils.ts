import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function isDemoMode(): boolean {
  return (
    typeof window !== "undefined" &&
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

export function jsonToYaml(obj: unknown, indentLevel = 0): string {
  const indent = "  ".repeat(indentLevel)
  if (obj === null || obj === undefined) return "null\n"
  if (typeof obj === "boolean" || typeof obj === "number") return `${obj}\n`
  if (typeof obj === "string") {
    if (
      obj.includes("\n") ||
      obj.includes("\r") ||
      obj.includes(":") ||
      obj.includes("#") ||
      obj.includes('"') ||
      obj.includes("\\")
    ) {
      return `${JSON.stringify(obj)}\n`
    }
    return `${obj}\n`
  }
  if (Array.isArray(obj)) {
    if (obj.length === 0) return "[]\n"
    let out = "\n"
    for (const item of obj) {
      out += `${indent}- ${jsonToYaml(item, indentLevel + 1).trim()}\n`
    }
    return out
  }
  if (typeof obj === "object") {
    const keys = Object.keys(obj as Record<string, unknown>)
    if (keys.length === 0) return "{}\n"
    let out = "\n"
    for (const key of keys) {
      const val = (obj as Record<string, unknown>)[key]
      if (typeof val === "object" && val !== null) {
        out += `${indent}${key}:${jsonToYaml(val, indentLevel + 1)}`
      } else {
        out += `${indent}${key}: ${jsonToYaml(val, 0)}`
      }
    }
    return out
  }
  return String(obj)
}
