import { describe, it, expect } from "vitest"
import {
  CANARY_HOST_VALUE,
  withBundleCanaryHostsRequired,
  withCanaryHostRequired,
} from "./canaryHost"

describe("withCanaryHostRequired", () => {
  const podinfo = {
    type: "object",
    required: ["replicaCount"],
    properties: {
      replicaCount: { type: "integer" },
      ingressHost: { type: "string", format: "hostname" },
    },
  }

  it("leaves the schema untouched when the canary is off", () => {
    expect(withCanaryHostRequired(podinfo, false)).toBe(podinfo)
  })

  it("requires a non-empty existing ingressHost without mutating the input", () => {
    const out = withCanaryHostRequired(podinfo, true) as typeof podinfo
    expect(out.required).toEqual(["replicaCount", CANARY_HOST_VALUE])
    expect(out.properties.ingressHost).toEqual({
      minLength: 1,
      type: "string",
      format: "hostname",
    })
    expect(podinfo.required).toEqual(["replicaCount"])
    expect(
      (podinfo.properties.ingressHost as Record<string, unknown>).minLength
    ).toBeUndefined()
    // Already required: not duplicated; an explicit minLength wins.
    const strict = {
      ...podinfo,
      required: ["ingressHost"],
      properties: { ingressHost: { type: "string", minLength: 4 } },
    }
    const again = withCanaryHostRequired(strict, true) as typeof strict
    expect(again.required).toEqual(["ingressHost"])
    expect(again.properties.ingressHost.minLength).toBe(4)
  })

  it("adds a Canary Hostname field when the schema has none", () => {
    const out = withCanaryHostRequired(
      { type: "object", properties: { replicas: { type: "integer" } } },
      true
    ) as unknown as {
      required: string[]
      properties: Record<string, { title?: string }>
    }
    expect(out.required).toEqual([CANARY_HOST_VALUE])
    expect(out.properties.replicas).toEqual({ type: "integer" })
    expect(out.properties.ingressHost.title).toBe("Canary Hostname")
    // No schema at all still yields an object schema with the field.
    const bare = withCanaryHostRequired(undefined, true) as unknown as {
      type: string
      required: string[]
    }
    expect(bare.type).toBe("object")
    expect(bare.required).toEqual([CANARY_HOST_VALUE])
  })
})

describe("withBundleCanaryHostsRequired", () => {
  const schema = {
    type: "object",
    required: ["cluster"],
    properties: {
      cluster: { type: "object", properties: { name: { type: "string" } } },
      web: { type: "object", properties: { replicas: { type: "integer" } } },
    },
  }

  it("requires the host of every canary app item only", () => {
    const out = withBundleCanaryHostsRequired(schema, [
      { id: "cluster", kind: "infra", catalogRef: "kind-cluster" },
      { id: "web", kind: "app", catalogRef: "podinfo", canary: true },
      { id: "api", kind: "app", catalogRef: "podinfo", canary: true },
      { id: "plain", kind: "app", catalogRef: "podinfo" },
    ]) as {
      required: string[]
      properties: Record<string, { required?: string[] }>
    }
    expect(out.required).toEqual(["cluster", "web", "api"])
    expect(out.properties.web.required).toEqual([CANARY_HOST_VALUE])
    expect(out.properties.api.required).toEqual([CANARY_HOST_VALUE])
    expect(out.properties.cluster.required).toBeUndefined()
    expect(out.properties.plain).toBeUndefined()
    expect(schema.required).toEqual(["cluster"])
  })

  it("is a no-op without canary items", () => {
    expect(withBundleCanaryHostsRequired(schema, undefined)).toBe(schema)
    expect(
      withBundleCanaryHostsRequired(schema, [
        { id: "web", kind: "app", catalogRef: "podinfo" },
      ])
    ).toBe(schema)
  })
})
