import { AuthStatus } from "../types/api"

const ADMIN_GROUPS = new Set(["keel-admins", "admin", "admins"])

/**
 * Client-side mirror of the server's `AuthSession` role checks (`crates/keel-server/src/auth.rs`).
 * Used only to hide/disable controls the server would reject; the server stays authoritative.
 */
export interface Authz {
  authenticated: boolean
  /** Cluster administrator (`keel-admins`/`admin`/`admins` group, or server-computed `isAdmin`, which covers allowlisted break-glass admins). */
  isAdmin: boolean
  /** Cluster admin or tenant admin (`tenant-<id>-admin`) for `tenantId`. */
  canAdminTenant: (tenantId?: string | null) => boolean
  /** Cluster admin, tenant admin, or tenant member (`tenant-<id>`; bare ids are not honored, matching the server) of `tenantId`. */
  canAccessTenant: (tenantId?: string | null) => boolean
}

/** Normalizes `tenant-acme` / `acme` to the tenant id `acme`. */
export function tenantIdFromNamespace(nsOrId?: string | null): string {
  const v = (nsOrId ?? "").trim()
  return v.startsWith("tenant-") ? v.slice("tenant-".length) : v
}

export function buildAuthz(status: AuthStatus | null | undefined): Authz {
  const authenticated = Boolean(status?.authenticated)
  const groups = authenticated ? (status?.groups ?? []) : []
  const isAdmin =
    authenticated &&
    (Boolean(status?.isAdmin) || groups.some((g) => ADMIN_GROUPS.has(g)))
  const canAdminTenant = (tenant?: string | null) => {
    if (isAdmin) return true
    const id = tenantIdFromNamespace(tenant)
    if (!authenticated || !id) return false
    return groups.includes(`tenant-${id}-admin`)
  }
  const canAccessTenant = (tenant?: string | null) => {
    if (canAdminTenant(tenant)) return true
    const id = tenantIdFromNamespace(tenant)
    if (!authenticated || !id) return false
    return groups.includes(`tenant-${id}`)
  }
  return { authenticated, isAdmin, canAdminTenant, canAccessTenant }
}

/** Fully-privileged authz used for demo mode and as the test default. */
export const ADMIN_AUTHZ: Authz = buildAuthz({
  authenticated: true,
  phase: "oidc-active",
  isAdmin: true,
})

/** No privileges (unauthenticated). */
export const ANONYMOUS_AUTHZ: Authz = buildAuthz(null)
