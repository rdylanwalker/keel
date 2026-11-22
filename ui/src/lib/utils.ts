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
  // Read `import.meta.env` keys directly so Vite folds the gate to a constant and drops the
  // demo fixture chunk from builds without `VITE_ENABLE_DEMO` (see `lib/demo.ts`).
  return import.meta.env.DEV || import.meta.env.VITE_ENABLE_DEMO === "true"
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

/**
 * Whether the stack exposes an SSH target. The server classifies every stack
 * (`resourceKind`); only VM stacks of SSH-capable providers qualify.
 */
export function isStackSshSupported(stack: {
  provider?: string
  resourceKind?: string
}): boolean {
  const provider = (stack.provider || "").trim().toLowerCase()
  if (NON_SSH_PROVIDERS.has(provider)) {
    return false
  }
  return stack.resourceKind === "vm"
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
