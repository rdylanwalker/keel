import { describe, expect, it } from "vitest"
import { parse } from "yaml"

import { DEFAULT_CATALOG } from "./demoFixtures"

// Node's fs is loaded through a non-literal specifier so the browser tsconfig needs no node types.
const fsModule = "node:fs"
const { existsSync, readFileSync } = (await import(
  /* @vite-ignore */ fsModule
)) as {
  existsSync: (file: URL) => boolean
  readFileSync: (file: URL, encoding: "utf8") => string
}

type Doc = {
  metadata: Record<string, unknown>
  spec: Record<string, unknown> & {
    upstream?: Record<string, unknown>
    placement?: { scope?: string; namespace?: string }
    progressiveDelivery?: {
      flaggerSupported?: boolean
      defaultCanary?: boolean
    }
    schema?: unknown
    uiSchema?: unknown
    items?: unknown[]
  }
}

const KIND_DIRS: Record<string, string> = {
  app: "apps",
  infra: "infra",
  bundle: "bundles",
}

function catalogFile(dir: string, id: string): URL {
  // Bind import.meta.url first so Vite's `new URL(…, import.meta.url)` asset rewrite skips it.
  const here = import.meta.url
  return new URL(`../../../deploy/catalog/${dir}/${id}/catalog.yaml`, here)
}

function loadDoc(dir: string, id: string): Doc {
  return parse(readFileSync(catalogFile(dir, id), "utf8")) as Doc
}

/**
 * Every value in `demo` must equal the value at the same path in `real` (demo schemas are
 * representative subsets). `required` and `ui:order` lists may be subsets.
 */
function expectSubset(demo: unknown, real: unknown, path: string): void {
  if (Array.isArray(demo)) {
    if (path.endsWith(".required") || path.endsWith(".ui:order")) {
      expect(Array.isArray(real), path).toBe(true)
      for (const entry of demo) {
        expect(real as unknown[], path).toContain(entry)
      }
      return
    }
    expect(real, path).toEqual(demo)
    return
  }
  if (demo && typeof demo === "object") {
    expect(real && typeof real === "object", path).toBe(true)
    for (const [key, value] of Object.entries(demo)) {
      expectSubset(
        value,
        (real as Record<string, unknown>)[key],
        `${path}.${key}`
      )
    }
    return
  }
  expect(real, path).toEqual(demo)
}

describe("DEFAULT_CATALOG mirrors deploy/catalog", () => {
  it.each(DEFAULT_CATALOG.map((item) => [item.id, item] as const))(
    "%s exists in the built-in catalog with matching metadata",
    (id, item) => {
      const dir = KIND_DIRS[item.type]
      expect(dir, `unexpected demo catalog type ${item.type}`).toBeDefined()
      expect(existsSync(catalogFile(dir, id)), `${dir}/${id}`).toBe(true)

      const doc = loadDoc(dir, id)
      expect(doc.metadata.id).toBe(id)
      expect(item.name).toBe(doc.metadata.name)
      expect(item.description).toBe(doc.metadata.description)
      expect(item.category).toBe(doc.metadata.category)

      if (item.type === "app") {
        const upstream = doc.spec.upstream ?? {}
        expect(item.version).toBe(upstream.version)
        expect(item.repoUrl).toBe(upstream.repoURL)
        expect(item.chart).toBe(upstream.chart)
        expect(item.path).toBe(upstream.path)
        expect(item.runtime).toBe(doc.metadata.runtime)
        const scope = doc.spec.placement?.scope
        expect(item.placementScope).toBe(
          scope === "cluster" ? "cluster" : undefined
        )
        expect(item.placementNamespace).toBe(
          scope === "cluster" ? doc.spec.placement?.namespace : undefined
        )
        expect(item.flaggerSupported).toBe(
          doc.spec.progressiveDelivery?.flaggerSupported ?? false
        )
        expect(item.defaultCanary).toBe(
          doc.spec.progressiveDelivery?.defaultCanary ?? false
        )
      } else if (item.type === "infra") {
        // `getCatalog` maps every blueprint to version 1.0.0 (blueprints are unversioned).
        expect(item.version).toBe("1.0.0")
        expect(item.provider).toBe(doc.metadata.provider)
        expect(item.resourceKind).toBe(doc.metadata.resourceKind)
        expect(item.pulumiRuntime).toBe(doc.spec.pulumiRuntime)
        expect(item.entrypoint).toBe(doc.spec.entrypoint)
        expect(item.protectByDefault).toBe(doc.spec.protectByDefault)
      } else {
        expect(item.version).toBe("bundle")
        expect(item.bundleItems).toEqual(doc.spec.items)
      }

      expectSubset(item.schema, doc.spec.schema, `${id}.schema`)
      if (item.uiSchema) {
        expectSubset(item.uiSchema, doc.spec.uiSchema, `${id}.uiSchema`)
      }
    }
  )

  it("mirrors the full proxmox-k3s-cluster form, not a subset", () => {
    // Demo mode (and screenshots) must show the same Proxmox form as a real install.
    const item = DEFAULT_CATALOG.find((i) => i.id === "proxmox-k3s-cluster")
    expect(item).toBeDefined()
    const doc = loadDoc("infra", "proxmox-k3s-cluster")
    expect(item?.schema).toEqual(doc.spec.schema)
    expect(item?.uiSchema).toEqual(doc.spec.uiSchema)
  })

  it("covers apps, infra blueprints, a bundle and at least one secret field", () => {
    const types = new Set(DEFAULT_CATALOG.map((item) => item.type))
    expect(types).toEqual(new Set(["app", "infra", "bundle"]))
    const json = JSON.stringify(DEFAULT_CATALOG.map((item) => item.schema))
    expect(json).toContain('"format":"password"')
  })

  it("references only catalog ids that exist in bundles", () => {
    for (const item of DEFAULT_CATALOG.filter((i) => i.type === "bundle")) {
      for (const entry of item.bundleItems ?? []) {
        const dir = entry.kind === "infra" ? "infra" : "apps"
        expect(
          existsSync(catalogFile(dir, entry.catalogRef)),
          `${item.id} -> ${dir}/${entry.catalogRef}`
        ).toBe(true)
      }
    }
  })
})
