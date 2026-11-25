import { describe, expect, it } from "vitest"

import { CatalogItem } from "@/types/api"
import {
  buildDeployProposalPayload,
  CUSTOM_CLUSTER,
  DeployTargetState,
  dedicatedNamespace,
  deployPlacementError,
  requiresRemoteCluster,
  resolveTargetCluster,
} from "./deployPayload"

const app: CatalogItem = {
  id: "flagger",
  name: "Flagger",
  description: "",
  type: "app",
  category: "progressive-delivery",
  version: "1.40.0",
  schema: { type: "object", properties: {} },
}
const target: DeployTargetState = {
  tenant: "acme",
  instanceName: "",
  chartVersion: "",
  targetCluster: "",
  customClusterSecret: "",
  targetNamespace: "",
  enableCanary: false,
  actionEngine: "flux",
}

describe("deploy placement helpers", () => {
  it("requires a remote cluster only for cluster-scoped apps", () => {
    expect(requiresRemoteCluster(app)).toBe(false)
    expect(requiresRemoteCluster({ ...app, placementScope: "cluster" })).toBe(
      true
    )
    expect(
      requiresRemoteCluster({
        ...app,
        type: "bundle",
        placementScope: "cluster",
      })
    ).toBe(false)
    expect(requiresRemoteCluster(null)).toBe(false)
  })

  it("resolves stack, custom, and management (local) targets", () => {
    expect(resolveTargetCluster(app, target)).toBeUndefined()
    expect(
      resolveTargetCluster(app, { ...target, targetCluster: "w-kubeconfig" })
    ).toBe("w-kubeconfig")
    expect(
      resolveTargetCluster(app, {
        ...target,
        targetCluster: CUSTOM_CLUSTER,
        customClusterSecret: "  edge-kubeconfig ",
      })
    ).toBe("edge-kubeconfig")
    expect(
      resolveTargetCluster(app, {
        ...target,
        targetCluster: CUSTOM_CLUSTER,
        customClusterSecret: "local",
      })
    ).toBeUndefined()
    expect(
      resolveTargetCluster(
        { ...app, type: "infra" },
        { ...target, targetCluster: "w-kubeconfig" }
      )
    ).toBeUndefined()
  })

  it("reports placement errors for cluster-scoped apps on the management cluster", () => {
    const clusterApp = { ...app, placementScope: "cluster" as const }
    expect(deployPlacementError(app, target)).toBeNull()
    expect(deployPlacementError(clusterApp, target)).toMatch(
      /Flagger installs cluster-scoped resources/
    )
    expect(
      deployPlacementError(clusterApp, {
        ...target,
        targetCluster: "w-kubeconfig",
      })
    ).toBeNull()
    expect(
      buildDeployProposalPayload(
        clusterApp,
        { ...target, targetCluster: "w-kubeconfig", targetNamespace: "fl" },
        {}
      )
    ).toMatchObject({ targetCluster: "w-kubeconfig", targetNamespace: "fl" })
  })

  it("pins cluster-scoped apps with a dedicated namespace to it", () => {
    const operator = {
      ...app,
      placementScope: "cluster" as const,
      placementNamespace: "flux-system",
    }
    expect(dedicatedNamespace(operator)).toBe("flux-system")
    expect(dedicatedNamespace({ ...app, placementNamespace: "x" })).toBe(
      undefined
    )
    expect(dedicatedNamespace(null)).toBeUndefined()
    for (const targetNamespace of ["", "tenant-acme"]) {
      expect(
        buildDeployProposalPayload(
          operator,
          { ...target, targetCluster: "w-kubeconfig", targetNamespace },
          {}
        )
      ).toMatchObject({
        targetCluster: "w-kubeconfig",
        targetNamespace: "flux-system",
      })
    }
    // Management cluster: no namespace (the placement error blocks submission anyway).
    expect(buildDeployProposalPayload(operator, target, {})).not.toHaveProperty(
      "targetNamespace"
    )
  })
})

describe("buildDeployProposalPayload not-secret overrides", () => {
  it("sends nonSecretPaths only when given (omitted keeps the persisted ones)", () => {
    expect(buildDeployProposalPayload(app, target, {})).not.toHaveProperty(
      "nonSecretPaths"
    )
    expect(
      buildDeployProposalPayload(app, target, {}, []).nonSecretPaths
    ).toEqual([])
    expect(
      buildDeployProposalPayload(app, target, {}, ["ingress.tlsSecret"])
        .nonSecretPaths
    ).toEqual(["ingress.tlsSecret"])
  })
})
