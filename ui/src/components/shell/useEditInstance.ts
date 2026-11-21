import { useCallback, useState } from "react"

import { CatalogItem, ReleaseSummary, StackSummary } from "../../types/api"
import {
  getCatalog,
  getReleaseConfig,
  getReleases,
  getStackConfig,
  getStacks,
} from "../../lib/api"
import { errorMessage } from "../../lib/errors"
import { DeployInstanceInitialConfig } from "../DeployInstanceModal"

export interface EditInstanceState {
  open: boolean
  loading: boolean
  error: string | null
  catalogItem: CatalogItem | null
  initialConfig: DeployInstanceInitialConfig | null
  stacks: StackSummary[]
  releases: ReleaseSummary[]
}

const CLOSED: EditInstanceState = {
  open: false,
  loading: false,
  error: null,
  catalogItem: null,
  initialConfig: null,
  stacks: [],
  releases: [],
}

/** Loads the live config + catalog blueprint for editing a topology stack/release in place. */
export function useEditInstance() {
  const [state, setState] = useState<EditInstanceState>(CLOSED)

  const open = useCallback(
    async (namespace: string, name: string, kind: "app" | "infra") => {
      setState({
        ...CLOSED,
        open: true,
        loading: true,
        initialConfig: { name },
      })
      try {
        const [cfg, catalogItems, stackList, releaseList] = await Promise.all([
          kind === "infra"
            ? getStackConfig(namespace, name)
            : getReleaseConfig(namespace, name),
          getCatalog().catch(() => [] as CatalogItem[]),
          getStacks().catch(() => [] as StackSummary[]),
          getReleases().catch(() => [] as ReleaseSummary[]),
        ])
        const matched =
          (catalogItems || []).find((item) => item.id === cfg.catalogId) ?? null
        setState({
          open: true,
          loading: false,
          error: matched
            ? null
            : `Catalog blueprint "${cfg.catalogId}" was not found for this instance.`,
          catalogItem: matched,
          stacks: (stackList || []).filter(
            (s) => s.name !== "management-cluster-core"
          ),
          releases: releaseList || [],
          initialConfig: {
            values: cfg.values,
            name: cfg.name || name,
            version: cfg.version || matched?.version || "",
            tenant: cfg.tenant || "default",
            targetCluster: cfg.targetCluster,
            targetNamespace: cfg.targetNamespace,
            canary: cfg.canary,
          },
        })
      } catch (err) {
        setState((prev) => ({
          ...prev,
          loading: false,
          error: errorMessage(err, "Failed to load instance configuration"),
        }))
      }
    },
    []
  )

  const close = useCallback(() => setState(CLOSED), [])

  return { state, open, close }
}
