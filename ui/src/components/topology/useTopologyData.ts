import { useCallback, useEffect, useRef, useState } from "react"

import { CrdWatchEvent, ReleaseSummary, StackSummary } from "../../types/api"
import {
  createEventSourceStream,
  getReleases,
  getStacksWithMeta,
} from "../../lib/api"
import { visibleLoadError } from "../../lib/errors"
import { isDemoMode } from "../../lib/utils"
import { demoFixtures } from "../../lib/demo"

/** Live-update transport state shown in the topology header (UI-17). */
export type StreamState = "demo" | "paused" | "connecting" | "live" | "polling"

/** Fallback polling schedule used only while the SSE stream is disconnected. */
export const POLL_BACKOFF_MS = [5000, 10000, 20000, 40000, 60000] as const
/** Debounce for refreshing the tables after an SSE CRD transition. */
export const SSE_REFRESH_DEBOUNCE_MS = 1500
const MAX_EVENTS = 100

function normalizeEvent(ev: CrdWatchEvent): CrdWatchEvent {
  const ts =
    ev.timestamp && !Number.isNaN(Date.parse(ev.timestamp))
      ? new Date(ev.timestamp).toLocaleTimeString()
      : ev.timestamp || new Date().toLocaleTimeString()
  return { ...ev, timestamp: ts }
}

function sameEvent(a: CrdWatchEvent, b: CrdWatchEvent): boolean {
  return (
    a.kind === b.kind &&
    a.namespace === b.namespace &&
    a.name === b.name &&
    a.status === b.status &&
    a.message === b.message &&
    (a.revision ?? null) === (b.revision ?? null) &&
    a.timestamp === b.timestamp
  )
}

export interface TopologyData {
  stacks: StackSummary[]
  releases: ReleaseSummary[]
  events: CrdWatchEvent[]
  loading: boolean
  error: string | null
  setError: (msg: string | null) => void
  refresh: () => Promise<void>
  scheduleRefresh: (delayMs: number) => void
  clearEvents: () => void
  streamState: StreamState
  /** Current fallback poll interval (ms) while `streamState === "polling"`. */
  pollIntervalMs: number | null
  paused: boolean
  setPaused: (paused: boolean) => void
}

/** `?tenant=` value that lists only the protected management add-ons (cluster admins only). */
export const MANAGEMENT_RELEASES_TENANT = "management"

/**
 * Loads stacks/releases and keeps them fresh. Updates are driven by the SSE CRD stream
 * (debounced refetch per transition); periodic polling with exponential backoff is used only
 * while the stream is disconnected. Demo mode never touches the network.
 *
 * With `includeManagementAddons` (cluster admins), the protected management add-ons are merged
 * into `releases` even when a tenant filter is active: the server only appends them to the
 * unfiltered list, so a tenant-scoped admin view fetches `?tenant=management` alongside.
 */
export function useTopologyData(
  authenticated: boolean,
  selectedTenant?: string,
  includeManagementAddons = false
): TopologyData {
  const demo = isDemoMode()
  const [stacks, setStacks] = useState<StackSummary[]>(() =>
    demo ? demoFixtures().DEFAULT_STACKS : []
  )
  const [releases, setReleases] = useState<ReleaseSummary[]>(() =>
    demo ? demoFixtures().DEMO_MANAGEMENT_ADDON_RELEASES : []
  )
  const [events, setEvents] = useState<CrdWatchEvent[]>(() =>
    demo ? demoFixtures().DEFAULT_EVENTS : []
  )
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [paused, setPaused] = useState(false)
  const [sseConnected, setSseConnected] = useState<boolean | null>(null)
  const [pollAttempt, setPollAttempt] = useState(0)

  const timeoutsRef = useRef<Set<number>>(new Set())
  const requestSeq = useRef(0)

  const refresh = useCallback(async () => {
    if (demo) return
    const seq = ++requestSeq.current
    setLoading(true)
    const fetchAddonsSeparately =
      includeManagementAddons &&
      Boolean(selectedTenant) &&
      selectedTenant !== MANAGEMENT_RELEASES_TENANT
    try {
      const [stacksRes, releasesData, addonsData] = await Promise.all([
        getStacksWithMeta(selectedTenant),
        getReleases(selectedTenant).catch(() => [] as ReleaseSummary[]),
        fetchAddonsSeparately
          ? getReleases(MANAGEMENT_RELEASES_TENANT).catch(
              () => [] as ReleaseSummary[]
            )
          : Promise.resolve([] as ReleaseSummary[]),
      ])
      if (seq !== requestSeq.current) return
      setError(null)
      setStacks(stacksRes.stacks || [])
      const addons = (addonsData || []).map((r) => ({
        ...r,
        managementAddon: true,
      }))
      setReleases([
        ...(releasesData || []).filter(
          (r) =>
            !addons.some(
              (a) => a.namespace === r.namespace && a.name === r.name
            )
        ),
        ...addons,
      ])
    } catch (err) {
      if (seq !== requestSeq.current) return
      setError(visibleLoadError(err, "Failed to load live stack topology"))
    } finally {
      if (seq === requestSeq.current) setLoading(false)
    }
  }, [demo, selectedTenant, includeManagementAddons])

  const scheduleRefresh = useCallback(
    (delayMs: number) => {
      const id = window.setTimeout(() => {
        timeoutsRef.current.delete(id)
        void refresh()
      }, delayMs)
      timeoutsRef.current.add(id)
    },
    [refresh]
  )

  // Initial load and reload on tenant change.
  useEffect(() => {
    if (!authenticated || demo) return
    const id = window.setTimeout(() => void refresh(), 0)
    return () => window.clearTimeout(id)
  }, [authenticated, demo, refresh])

  // Clear pending timers on unmount.
  useEffect(() => {
    const timeouts = timeoutsRef.current
    return () => {
      timeouts.forEach((id) => window.clearTimeout(id))
      timeouts.clear()
    }
  }, [])

  // SSE stream: append events and refresh tables (debounced) on each transition.
  useEffect(() => {
    if (!authenticated || paused || demo) return
    let debounce: number | undefined
    let es: EventSource | null = null
    const markLive = () => {
      setSseConnected(true)
      setPollAttempt(0)
    }
    try {
      es = createEventSourceStream(
        (incoming) => {
          markLive()
          const ev = normalizeEvent(incoming)
          setEvents((prev) =>
            prev.some((e) => sameEvent(e, ev))
              ? prev
              : [ev, ...prev.slice(0, MAX_EVENTS - 1)]
          )
          window.clearTimeout(debounce)
          debounce = window.setTimeout(
            () => void refresh(),
            SSE_REFRESH_DEBOUNCE_MS
          )
        },
        () => setSseConnected(false)
      )
      if (es) es.onopen = markLive
    } catch {
      // EventSource unavailable: fall back to polling (deferred to avoid a sync render cascade).
      debounce = window.setTimeout(() => setSseConnected(false), 0)
    }
    return () => {
      window.clearTimeout(debounce)
      es?.close()
      setSseConnected(null)
    }
  }, [authenticated, paused, demo, refresh])

  const polling = authenticated && !demo && (paused || sseConnected === false)
  const pollIntervalMs = polling
    ? POLL_BACKOFF_MS[Math.min(pollAttempt, POLL_BACKOFF_MS.length - 1)]
    : null

  // Fallback polling (only while SSE is disconnected or paused), with backoff.
  useEffect(() => {
    if (pollIntervalMs === null) return
    const id = window.setTimeout(() => {
      void refresh()
      setPollAttempt((n) => n + 1)
    }, pollIntervalMs)
    return () => window.clearTimeout(id)
  }, [pollIntervalMs, pollAttempt, refresh])

  const streamState: StreamState = demo
    ? "demo"
    : paused
      ? "paused"
      : sseConnected === false
        ? "polling"
        : sseConnected
          ? "live"
          : "connecting"

  return {
    stacks,
    releases,
    events,
    loading,
    error,
    setError,
    refresh,
    scheduleRefresh,
    clearEvents: () => setEvents([]),
    streamState,
    pollIntervalMs,
    paused,
    setPaused,
  }
}
