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
