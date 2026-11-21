import { useEffect, useRef, useState } from "react"

import { getCatalog, getReleases, getStacks, getTenants } from "@/lib/api"
import { DEFAULT_CATALOG, DEMO_TENANTS } from "@/lib/demoFixtures"
import { visibleLoadError } from "@/lib/errors"
import { isDemoMode } from "@/lib/utils"
import {
  CatalogItem,
  ReleaseSummary,
  StackSummary,
  TenantSummary,
} from "@/types/api"

const FALLBACK_TENANTS: TenantSummary[] = [
  { id: "default", namespace: "tenant-default" },
]

/** Runs `fn` so that synchronous throws also surface as promise rejections. */
const attempt = <T>(fn: () => Promise<T>): Promise<T> =>
  Promise.resolve().then(fn)

/**
 * Loads catalog items, tenants, stacks and releases for the catalog wizard. In demo mode no
 * API calls are issued and bundled fixtures are used instead.
 */
export function useCatalogData(
  onTenantsLoaded?: (items: TenantSummary[]) => void
) {
  const demo = isDemoMode()
  const [catalog, setCatalog] = useState<CatalogItem[]>(() =>
    demo ? DEFAULT_CATALOG : []
  )
  const [tenants, setTenants] = useState<TenantSummary[]>(() =>
    demo ? DEMO_TENANTS : FALLBACK_TENANTS
  )
  const [stacks, setStacks] = useState<StackSummary[]>([])
  const [releases, setReleases] = useState<ReleaseSummary[]>([])
  const [loadError, setLoadError] = useState<string | null>(null)
  const onTenantsLoadedRef = useRef(onTenantsLoaded)
  useEffect(() => {
    onTenantsLoadedRef.current = onTenantsLoaded
  }, [onTenantsLoaded])

  useEffect(() => {
    if (isDemoMode()) return
    let cancelled = false
    attempt(getTenants)
      .then((items) => {
        if (cancelled) return
        if (items && items.length > 0) {
          setTenants(items)
          onTenantsLoadedRef.current?.(items)
        } else {
          setTenants(FALLBACK_TENANTS)
        }
      })
      .catch(() => !cancelled && setTenants(FALLBACK_TENANTS))
    attempt(getStacks)
      .then(
        (list) =>
          !cancelled &&
          setStacks(
            (list || []).filter((s) => s.name !== "management-cluster-core")
          )
      )
      .catch(() => !cancelled && setStacks([]))
    attempt(getReleases)
      .then((list) => !cancelled && setReleases(list || []))
      .catch(() => !cancelled && setReleases([]))
    attempt(getCatalog)
      .then((items) => !cancelled && setCatalog(items ?? []))
      .catch((err) => {
        if (cancelled) return
        setLoadError(visibleLoadError(err, "Failed to load catalog"))
        setCatalog([])
      })
    return () => {
      cancelled = true
    }
  }, [])

  return {
    catalog,
    setCatalog,
    tenants,
    setTenants,
    stacks,
    releases,
    loadError,
  }
}
