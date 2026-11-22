import { fireEvent, render, screen, within } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"

import { CreateTenantResult } from "../../types/api"
import {
  credentialKind,
  TenantProvisioningSummary,
  tenantProvisioningSteps,
} from "./TenantProvisioningSummary"

const base = (
  repository: Partial<CreateTenantResult["repository"]> = {},
  proposal: CreateTenantResult["proposal"] = undefined
): CreateTenantResult => ({
  tenant: { id: "acme", namespace: "tenant-acme" },
  proposal,
  repository: {
    layout: "dedicated",
    owner: "keel-tenants",
    name: "infra-tenant-acme",
    transport: "https",
    cluster: {
      namespaceReady: true,
      sopsAge: "existed",
      credential: "rotated",
    },
    notes: [],
    ...repository,
  },
})

describe("tenantProvisioningSteps", () => {
  it("describes credential kinds without exposing values", () => {
    expect(credentialKind("https")).toBe("Repository-scoped HTTPS read token")
    expect(credentialKind("ssh")).toBe("Read-only SSH deploy key")
    expect(credentialKind(undefined)).toBe("Repository read credential")
  })

  it("flags forge settings that need an operator", () => {
    const steps = tenantProvisioningSteps(
      base({
        mainProtection: "unsupportedPlan",
        forks: {
          result: "requiresAdminSetting",
          detail: "Disable forks in the organisation settings",
        },
      })
    )
    const byId = Object.fromEntries(steps.map((s) => [s.id, s]))
    expect(byId.protection.tone).toBe("warning")
    expect(byId.forks.tone).toBe("warning")
    expect(byId.forks.detail).toBe("Disable forks in the organisation settings")
    expect(byId.credential.tone).toBe("done")
    expect(String(byId.credential.detail)).toMatch(/was replaced/)
    expect(byId.sops.detail).toMatch(/already present in tenant-acme/)
  })

  it("reports already-configured forge settings as done", () => {
    const steps = tenantProvisioningSteps(
      base({
        mainProtection: "unchanged",
        forks: { result: "alreadyDisabled" },
      })
    )
    const byId = Object.fromEntries(steps.map((s) => [s.id, s]))
    expect(byId.protection.tone).toBe("done")
    expect(byId.protection.detail).toMatch(/Already protected/)
    expect(byId.forks.detail).toMatch(/already disabled/)
  })

  it("marks the Local Git provider layout as development-only", () => {
    const steps = tenantProvisioningSteps(
      base({
        layout: "monorepo",
        owner: undefined,
        name: undefined,
        transport: undefined,
        cluster: {
          namespaceReady: false,
          sopsAge: "created",
          credential: "notApplicable",
        },
      })
    )
    expect(steps.map((s) => s.id)).toEqual(["repository", "sops"])
    expect(steps[0].tone).toBe("warning")
    expect(steps[0].detail).toMatch(/no per-tenant repository isolation/)
    expect(steps[1].tone).toBe("done")
  })
})

describe("TenantProvisioningSummary", () => {
  it("explains a re-run whose platform stub is already on main", () => {
    render(<TenantProvisioningSummary result={base({ created: false })} />)
    const summary = screen.getByTestId("tenant-provisioning-summary")
    expect(summary).toHaveTextContent(/stub is already on main/)
    expect(
      within(summary).getByTestId("tenant-step-repository")
    ).toHaveTextContent(/already existed and was adopted/)
    expect(
      screen.queryByRole("link", { name: /Open tenant repository/i })
    ).not.toBeInTheDocument()
    expect(screen.queryByTestId("tenant-provisioning-notes")).toBeNull()
  })

  it("links the platform PR to the forge and the inspector", () => {
    const onNavigate = vi.fn()
    render(
      <TenantProvisioningSummary
        onNavigateToPreview={onNavigate}
        result={base(
          { webUrl: "https://git.example/keel-tenants/infra-tenant-acme" },
          {
            id: "12",
            title: "create tenant acme",
            branch: "keel/acme/create-tenant-12",
            tenant: "acme",
            scope: "management",
            status: "open",
            createdAt: "now",
            prUrl: "https://git.example/keel/infra/pulls/12",
          }
        )}
      />
    )
    expect(screen.getByText(/Platform tenant stub: PR #12/)).toBeInTheDocument()
    expect(screen.getByRole("link", { name: /Forge/ })).toHaveAttribute(
      "href",
      "https://git.example/keel/infra/pulls/12"
    )
    fireEvent.click(screen.getByRole("button", { name: /Go to PR/ }))
    expect(onNavigate).toHaveBeenCalledWith({ scope: "management", id: "12" })
  })
})
