import { describe, expect, it } from "vitest"

import { repositoryWebUrl, safeHttpUrl } from "./forgeLinks"

describe("forge links", () => {
  it("accepts only http(s) URLs and strips embedded credentials", () => {
    expect(safeHttpUrl("https://forge.example/o/r/pulls/1")).toBe(
      "https://forge.example/o/r/pulls/1"
    )
    expect(safeHttpUrl("http://127.0.0.1:3000/o/r")).toBe(
      "http://127.0.0.1:3000/o/r"
    )
    expect(safeHttpUrl("https://bot:secret@forge.example/o/r")).toBe(
      "https://forge.example/o/r"
    )
    expect(safeHttpUrl("javascript:alert(1)")).toBeUndefined()
    expect(safeHttpUrl("local:///tmp/remote.git/.keel/proposals/1.json")).toBe(
      undefined
    )
    expect(safeHttpUrl("not a url")).toBeUndefined()
    expect(safeHttpUrl(undefined)).toBeUndefined()
    expect(safeHttpUrl("")).toBeUndefined()
  })

  it("derives the repository page from Forgejo, GitHub and GitLab PR URLs", () => {
    expect(
      repositoryWebUrl(
        "https://git.example/keel-tenants/infra-tenant-acme/pulls/7"
      )
    ).toBe("https://git.example/keel-tenants/infra-tenant-acme")
    expect(
      repositoryWebUrl("https://github.com/acme/tenant-acme/pull/12")
    ).toBe("https://github.com/acme/tenant-acme")
    expect(
      repositoryWebUrl(
        "https://gitlab.example/group/sub/tenant-acme/-/merge_requests/3"
      )
    ).toBe("https://gitlab.example/group/sub/tenant-acme")
    expect(repositoryWebUrl("https://u:p@git.example/o/r/pulls/7")).toBe(
      "https://git.example/o/r"
    )
    expect(repositoryWebUrl("https://git.example/o/r/issues/7")).toBeUndefined()
    expect(repositoryWebUrl("https://git.example/pulls/7")).toBeUndefined()
    expect(
      repositoryWebUrl("local:///tmp/remote.git/.keel/proposals/7.json")
    ).toBeUndefined()
    expect(repositoryWebUrl(undefined)).toBeUndefined()
  })
})
