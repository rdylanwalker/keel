import { describe, expect, it } from "vitest"
import {
  ADMIN_AUTHZ,
  ANONYMOUS_AUTHZ,
  buildAuthz,
  tenantIdFromNamespace,
} from "./authz"

describe("buildAuthz (mirror of server AuthSession)", () => {
  it("anonymous has no rights", () => {
    expect(ANONYMOUS_AUTHZ.isAdmin).toBe(false)
    expect(ANONYMOUS_AUTHZ.canAccessTenant("acme")).toBe(false)
    expect(
      buildAuthz({
        authenticated: false,
        phase: "oidc-active",
        isAdmin: true,
        groups: ["keel-admins"],
      }).isAdmin
    ).toBe(false)
  })

  it("admins via flag, group, or break-glass", () => {
    expect(ADMIN_AUTHZ.canAdminTenant("anything")).toBe(true)
    for (const g of ["keel-admins", "admin", "admins"]) {
      expect(
        buildAuthz({ authenticated: true, phase: "oidc", groups: [g] }).isAdmin
      ).toBe(true)
    }
    // Break-glass admin is decided server-side (KEEL_BREAK_GLASS_ADMINS allowlist) and surfaced
    // via `isAdmin`; the phase alone must not grant admin in the UI.
    expect(
      buildAuthz({ authenticated: true, phase: "break-glass" }).isAdmin
    ).toBe(false)
    expect(
      buildAuthz({ authenticated: true, phase: "break-glass", isAdmin: true })
        .isAdmin
    ).toBe(true)
  })

  it("tenant admin and member groups", () => {
    const tenantAdmin = buildAuthz({
      authenticated: true,
      phase: "oidc",
      groups: ["tenant-acme-admin", "globex-admin"],
    })
    expect(tenantAdmin.isAdmin).toBe(false)
    expect(tenantAdmin.canAdminTenant("acme")).toBe(true)
    // Bare `<id>-admin` groups are not honored (server requires `tenant-<id>-admin`).
    expect(tenantAdmin.canAdminTenant("tenant-globex")).toBe(false)
    expect(tenantAdmin.canAdminTenant("other")).toBe(false)
    expect(tenantAdmin.canAdminTenant("")).toBe(false)
    expect(tenantAdmin.canAccessTenant("tenant-acme")).toBe(true)

    const member = buildAuthz({
      authenticated: true,
      phase: "oidc",
      groups: ["acme", "tenant-beta"],
    })
    expect(member.canAdminTenant("acme")).toBe(false)
    // Bare `<id>` groups are not honored (server requires `tenant-<id>`).
    expect(member.canAccessTenant("acme")).toBe(false)
    expect(member.canAccessTenant("beta")).toBe(true)
    expect(member.canAccessTenant("flux-system")).toBe(false)
    expect(member.canAccessTenant(null)).toBe(false)
  })

  it("normalizes tenant namespaces", () => {
    expect(tenantIdFromNamespace("tenant-acme")).toBe("acme")
    expect(tenantIdFromNamespace(" acme ")).toBe("acme")
    expect(tenantIdFromNamespace(undefined)).toBe("")
  })
})
