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

export function isStackSshSupported(stack: {
  name?: string
  provider?: string
  type?: string
}): boolean {
  const provider = (stack.provider || "").trim().toLowerCase()
  if (NON_SSH_PROVIDERS.has(provider)) {
    return false
  }
  const name = (stack.name || "").trim().toLowerCase()
  if (
    (name === "management-cluster-core" || stack.type === "management") &&
    !provider
  ) {
    return false
  }
  if (name === "postgres-managed" || name === "canary-gateway-config") {
    return false
  }
  return true
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
