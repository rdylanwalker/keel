import { afterEach, describe, expect, it, vi } from "vitest"
import {
  consumePostLoginLocation,
  DemoModeError,
  getStacks,
  rememberPostLoginLocation,
  startOidcLogin,
} from "./api"
import { ApiError } from "./errors"

describe("api demo guard and post-login deep link", () => {
  afterEach(() => {
    window.history.replaceState(null, "", "/")
    window.sessionStorage.clear()
    vi.unstubAllGlobals()
  })

  it("never calls fetch in demo mode", async () => {
    const fetchMock = vi.fn()
    vi.stubGlobal("fetch", fetchMock)
    window.history.replaceState(null, "", "/?demo=true")
    await expect(getStacks()).rejects.toBeInstanceOf(DemoModeError)
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it("throws structured ApiError with correlation id", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 500,
        statusText: "Internal Server Error",
        text: async () =>
          JSON.stringify({
            error: "Internal server error",
            correlationId: "z",
          }),
      })
    )
    const err = await getStacks().catch((e) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err.correlationId).toBe("z")
  })

  it("stores and restores the hash deep link around OIDC login", () => {
    const assign = vi.fn()
    vi.stubGlobal("location", {
      ...window.location,
      hash: "#preview/12",
      assign,
    })
    startOidcLogin()
    expect(assign).toHaveBeenCalledWith("/api/v1/auth/login")
    vi.unstubAllGlobals()
    expect(consumePostLoginLocation()).toBe("#preview/12")
    expect(consumePostLoginLocation()).toBeNull()
  })

  it("ignores empty or unsafe stored hashes", () => {
    window.history.replaceState(null, "", "/")
    rememberPostLoginLocation()
    expect(consumePostLoginLocation()).toBeNull()
    window.sessionStorage.setItem("keel:post-login-hash", '#x"><img>')
    expect(consumePostLoginLocation()).toBeNull()
  })
})
