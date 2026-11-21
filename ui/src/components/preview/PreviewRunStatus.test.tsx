import { render, screen, renderHook, act } from "@testing-library/react"
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest"

import * as api from "../../lib/api"
import { CrdWatchEvent } from "../../types/api"
import { isPreviewRunActive, PreviewRunStatus } from "./PreviewRunStatus"
import {
  PREVIEW_POLL_INITIAL_MS,
  usePreviewRunRefresh,
} from "./usePreviewRunRefresh"
import { shouldShowPolicyBanner } from "./PolicyStatusBanner"

vi.mock("../../lib/api", () => ({
  createEventSourceStream: vi.fn(),
}))

describe("PreviewRunStatus", () => {
  it("renders nothing without a run", () => {
    const { container } = render(<PreviewRunStatus />)
    expect(container).toBeEmptyDOMElement()
  })

  it("renders each phase", () => {
    const { rerender } = render(<PreviewRunStatus run={{ phase: "pending" }} />)
    expect(screen.getByTestId("preview-run-status")).toHaveTextContent(
      "PKO preview pending"
    )
    rerender(<PreviewRunStatus run={{ phase: "running", message: "step 2" }} />)
    expect(screen.getByTestId("preview-run-status")).toHaveTextContent(
      "PKO preview running — step 2"
    )
    rerender(<PreviewRunStatus run={{ phase: "failed", message: "boom" }} />)
    expect(screen.getByRole("alert")).toHaveTextContent(
      "PKO preview failed: boom"
    )
    rerender(
      <PreviewRunStatus
        run={{ phase: "notExecuted", message: "no infra changes" }}
      />
    )
    expect(screen.getByTestId("preview-run-status")).toHaveTextContent(
      "Not executed: no infra changes"
    )
    rerender(
      <PreviewRunStatus
        run={{
          phase: "succeeded",
          stack: "keel-previews/pr-7",
          commit: "0123456789abcdef",
        }}
      />
    )
    const ok = screen.getByTestId("preview-run-status")
    expect(ok).toHaveTextContent("PKO preview succeeded")
    expect(ok).toHaveTextContent("keel-previews/pr-7")
    expect(ok).toHaveTextContent("0123456789ab")
    expect(ok).not.toHaveTextContent("0123456789abcdef")
  })

  it("classifies active phases and policy visibility", () => {
    expect(isPreviewRunActive({ phase: "pending" })).toBe(true)
    expect(isPreviewRunActive({ phase: "running" })).toBe(true)
    expect(isPreviewRunActive({ phase: "failed" })).toBe(false)
    expect(isPreviewRunActive(undefined)).toBe(false)
    expect(
      shouldShowPolicyBanner({
        source: "pko",
        policyStatus: "passed",
        run: { phase: "notExecuted" },
      })
    ).toBe(false)
    expect(
      shouldShowPolicyBanner({
        source: "pko",
        policyStatus: "passed",
        run: { phase: "succeeded" },
      })
    ).toBe(true)
  })
})

describe("usePreviewRunRefresh", () => {
  let onEvent: ((e: CrdWatchEvent) => void) | undefined
  let onError: ((e: Event) => void) | undefined
  const close = vi.fn()

  beforeEach(() => {
    vi.useFakeTimers()
    close.mockReset()
    vi.mocked(api.createEventSourceStream).mockReset()
    vi.mocked(api.createEventSourceStream).mockImplementation((ev, err) => {
      onEvent = ev
      onError = err
      return { close } as unknown as EventSource
    })
  })
  afterEach(() => {
    vi.useRealTimers()
    window.history.replaceState({}, "", "/")
  })

  const event = (message: string): CrdWatchEvent => ({
    kind: "Stack",
    namespace: "keel-previews",
    name: "pr-7",
    status: "Progressing",
    message,
    timestamp: "now",
  })

  it("does nothing while inactive", () => {
    const refresh = vi.fn()
    renderHook(() => usePreviewRunRefresh(false, refresh))
    expect(api.createEventSourceStream).not.toHaveBeenCalled()
  })

  it("does nothing in demo mode", () => {
    window.history.replaceState({}, "", "/?demo=true")
    renderHook(() => usePreviewRunRefresh(true, vi.fn()))
    expect(api.createEventSourceStream).not.toHaveBeenCalled()
  })

  it("refreshes on PKO preview SSE events only and closes on unmount", () => {
    const refresh = vi.fn()
    const { unmount } = renderHook(() => usePreviewRunRefresh(true, refresh))
    act(() => onEvent!(event("Reconciliation in progress")))
    expect(refresh).not.toHaveBeenCalled()
    act(() => onEvent!(event("PKO preview succeeded")))
    expect(refresh).toHaveBeenCalledTimes(1)
    // No polling while SSE is healthy.
    act(() => vi.advanceTimersByTime(60000))
    expect(refresh).toHaveBeenCalledTimes(1)
    unmount()
    expect(close).toHaveBeenCalled()
  })

  it("falls back to polling with backoff after SSE errors and stops when SSE resumes", () => {
    const refresh = vi.fn()
    renderHook(() => usePreviewRunRefresh(true, refresh))
    act(() => onError!(new Event("error")))
    act(() => vi.advanceTimersByTime(PREVIEW_POLL_INITIAL_MS))
    expect(refresh).toHaveBeenCalledTimes(1)
    act(() => vi.advanceTimersByTime(PREVIEW_POLL_INITIAL_MS))
    expect(refresh).toHaveBeenCalledTimes(1) // backed off to 10s
    act(() => vi.advanceTimersByTime(PREVIEW_POLL_INITIAL_MS))
    expect(refresh).toHaveBeenCalledTimes(2)
    act(() => onEvent!(event("unrelated")))
    act(() => vi.advanceTimersByTime(120000))
    expect(refresh).toHaveBeenCalledTimes(2)
  })

  it("polls when the event stream cannot be created", () => {
    vi.mocked(api.createEventSourceStream).mockImplementation(() => {
      throw new Error("no EventSource")
    })
    const refresh = vi.fn()
    const { unmount } = renderHook(() => usePreviewRunRefresh(true, refresh))
    act(() => vi.advanceTimersByTime(PREVIEW_POLL_INITIAL_MS))
    expect(refresh).toHaveBeenCalledTimes(1)
    unmount()
    act(() => vi.advanceTimersByTime(120000))
    expect(refresh).toHaveBeenCalledTimes(1)
  })
})
