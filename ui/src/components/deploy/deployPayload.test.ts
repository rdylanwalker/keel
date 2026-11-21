import { describe, expect, it } from "vitest"

import { CatalogItem } from "@/types/api"
import {
  buildDeployProposalPayload,
  CUSTOM_CLUSTER,
  DeployTargetState,
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
})
