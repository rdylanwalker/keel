import { describe, it, expect, vi, afterEach } from "vitest"

import { getProposalPreview, getProposals } from "./api"

function stubJson(body: unknown) {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({ ok: true, json: async () => body })
  )
}

describe("preview run + camelCase PullRequestInfo normalization", () => {
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it("passes report.run through getProposalPreview", async () => {
    stubJson({
      url: "https://forge/pr/9",
      report: {
        source: "pko",
        summary: { create: 0, update: 0, delete: 0, replace: 0, same: 0 },
        run: {
          phase: "running",
          message: "pulumi preview",
          stack: "keel-previews/pr-9",
        },
      },
    })
    const preview = await getProposalPreview({ scope: "acme", id: "9" })
    expect(preview.source).toBe("pko")
    expect(preview.run).toEqual({
      phase: "running",
      message: "pulumi preview",
      stack: "keel-previews/pr-9",
    })
  })

  it("normalizes camelCase resource diffs", async () => {
    stubJson({
      report: {
        source: "pko",
        summary: { create: 1, update: 1, delete: 0, replace: 0, same: 0 },
        resources: [
          {
            urn: "urn:a",
            name: "vm",
            op: "create",
            resourceType: "aws:ec2/instance:Instance",
            changedFields: ["tags"],
          },
          {
            urn: "urn:b",
            name: "cm",
            op: "update",
            resourceType: "kubernetes:core/v1:ConfigMap",
          },
        ],
      },
    })
    const preview = await getProposalPreview({ scope: "acme", id: "9" })
    expect(preview.resources).toEqual([
      {
        urn: "urn:a",
        name: "vm",
        op: "create",
        resource_type: "aws:ec2/instance:Instance",
        changed_fields: ["tags"],
      },
      {
        urn: "urn:b",
        name: "cm",
        op: "update",
        resource_type: "kubernetes:core/v1:ConfigMap",
        changed_fields: [],
      },
    ])
  })

  it("normalizes camelCase PullRequestInfo from the forge", async () => {
    stubJson({
      proposals: [
        {
          id: "12",
          title: "feat(acme): deploy podinfo",
          scope: "acme",
          sourceBranch: "keel/acme/podinfo-12",
          state: "open",
          createdAt: "2026-11-20T10:00:00Z",
          url: "https://forge/pr/12",
        },
      ],
    })
    const [pr] = await getProposals()
    expect(pr).toMatchObject({
      id: "12",
      title: "feat(acme): deploy podinfo",
      branch: "keel/acme/podinfo-12",
      status: "open",
      createdAt: "2026-11-20T10:00:00Z",
      prUrl: "https://forge/pr/12",
    })
  })
})
