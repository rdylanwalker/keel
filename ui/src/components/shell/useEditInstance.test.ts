import { act, renderHook } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"

import * as api from "../../lib/api"
import { useEditInstance } from "./useEditInstance"

vi.mock("../../lib/api", () => ({
  getCatalog: vi.fn(),
  getReleaseConfig: vi.fn(),
  getReleases: vi.fn(),
  getStackConfig: vi.fn(),
  getStacks: vi.fn(),
}))

describe("useEditInstance", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(api.getCatalog).mockResolvedValue([
      {
        id: "mailer",
        name: "Mailer",
        description: "",
        type: "app",
        category: "Apps",
        version: "1.0.0",
        schema: { type: "object" },
      },
    ])
    vi.mocked(api.getStacks).mockResolvedValue([])
    vi.mocked(api.getReleases).mockResolvedValue([])
  })

  it("passes the server's masked values and persisted overrides to the edit form", async () => {
    vi.mocked(api.getReleaseConfig).mockResolvedValue({
      catalogId: "mailer",
      tenant: "acme",
      name: "mailer",
      values: { smtp: { host: "mail.local" } },
      nonSecretPaths: ["smtp.tlsSecret"],
    })
    const { result } = renderHook(() => useEditInstance())
    await act(() => result.current.open("tenant-acme", "mailer", "app"))
    expect(api.getReleaseConfig).toHaveBeenCalledWith("tenant-acme", "mailer")
    expect(result.current.state.error).toBeNull()
    expect(result.current.state.initialConfig).toMatchObject({
      values: { smtp: { host: "mail.local" } },
      tenant: "acme",
      nonSecretPaths: ["smtp.tlsSecret"],
    })
  })

  it("loads stack configs for infrastructure instances", async () => {
    vi.mocked(api.getStackConfig).mockResolvedValue({
      catalogId: "mailer",
      tenant: "acme",
      name: "vm",
      values: {},
      nonSecretPaths: [],
    })
    const { result } = renderHook(() => useEditInstance())
    await act(() => result.current.open("tenant-acme", "vm", "infra"))
    expect(api.getStackConfig).toHaveBeenCalledWith("tenant-acme", "vm")
    expect(result.current.state.initialConfig?.nonSecretPaths).toEqual([])
  })
})
