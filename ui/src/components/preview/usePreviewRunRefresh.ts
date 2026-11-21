import { useEffect, useRef } from "react"

import { createEventSourceStream } from "../../lib/api"
import { isDemoMode } from "../../lib/utils"

export const PREVIEW_POLL_INITIAL_MS = 5000
export const PREVIEW_POLL_MAX_MS = 30000

/**
 * While a PKO preview run is pending/running, calls `onRefresh` whenever the SSE stream
 * reports a `PKO preview…` event. Polling (5s, doubling up to 30s) is used only as a
 * fallback after the SSE stream errors, and stops again once SSE events resume (UI-17).
 * Never active in demo mode (no live API calls).
 */
export function usePreviewRunRefresh(active: boolean, onRefresh: () => void) {
  const callbackRef = useRef(onRefresh)
  useEffect(() => {
    callbackRef.current = onRefresh
  }, [onRefresh])

  useEffect(() => {
    if (!active || isDemoMode()) return
    let closed = false
    let timer: number | undefined
    let delay = PREVIEW_POLL_INITIAL_MS

    const stopPolling = () => {
      if (timer !== undefined) window.clearTimeout(timer)
      timer = undefined
      delay = PREVIEW_POLL_INITIAL_MS
    }
    const schedulePoll = () => {
      if (closed || timer !== undefined) return
      timer = window.setTimeout(() => {
        timer = undefined
        if (closed) return
        callbackRef.current()
        delay = Math.min(delay * 2, PREVIEW_POLL_MAX_MS)
        schedulePoll()
      }, delay)
    }

    let es: EventSource | null = null
    try {
      es = createEventSourceStream(
        (event) => {
          stopPolling()
          if ((event.message ?? "").startsWith("PKO preview")) {
            callbackRef.current()
          }
        },
        () => schedulePoll()
      )
    } catch {
      schedulePoll()
    }

    return () => {
      closed = true
      stopPolling()
      es?.close()
    }
  }, [active])
}
