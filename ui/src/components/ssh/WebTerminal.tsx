import React, { useEffect, useRef } from "react"
import { WTerm } from "@wterm/dom"
import "@wterm/dom/src/terminal.css"

/** Maximum number of lines retained in the terminal scrollback buffer. */
export const TERMINAL_SCROLLBACK_LINES = 5000

/** Fallback geometry until the terminal has measured its container. */
const DEFAULT_SIZE = { cols: 80, rows: 24 }

export interface TerminalHandle {
  write: (data: string) => void
  focus: () => void
  /** Current terminal geometry (columns × rows). */
  size: () => { cols: number; rows: number }
}

export interface WebTerminalProps {
  /** Raw keystrokes / pasted data typed by the user (already VT-encoded). */
  onData: (data: string) => void
  /** Called whenever the fitted terminal geometry changes (forwarded as PTY window changes). */
  onResize?: (cols: number, rows: number) => void
  /** Called once the terminal is initialized and ready to receive writes. */
  onReady?: (handle: TerminalHandle) => void
  className?: string
  "data-testid"?: string
}

/**
 * Browser SSH terminal surface backed by wterm (`@wterm/dom`): a small WASM VT core with a
 * DOM renderer, so output stays selectable, searchable with the browser's find, and readable
 * by assistive technology. Streams raw keystrokes, renders ANSI output, keeps a bounded
 * scrollback, and auto-fits to its container (reported via `onResize`).
 */
export const WebTerminal: React.FC<WebTerminalProps> = ({
  onData,
  onResize,
  onReady,
  className,
  "data-testid": testId,
}) => {
  const containerRef = useRef<HTMLDivElement | null>(null)
  const callbacks = useRef({ onData, onResize, onReady })

  useEffect(() => {
    callbacks.current = { onData, onResize, onReady }
  }, [onData, onResize, onReady])

  useEffect(() => {
    const container = containerRef.current
    if (!container) return

    let disposed = false
    let size = { ...DEFAULT_SIZE }
    const term = new WTerm(container, {
      scrollbackLines: TERMINAL_SCROLLBACK_LINES,
      autoResize: true,
      cursorBlink: true,
      onData: (d) => callbacks.current.onData(d),
      onResize: (cols, rows) => {
        size = { cols, rows }
        callbacks.current.onResize?.(cols, rows)
      },
    })

    term
      .init()
      .then(() => {
        if (disposed) return
        try {
          // Measure the container now so the first `connect` frame carries real geometry.
          term.fit()
        } catch {
          // Container not laid out yet (hidden/zero-size); autoResize catches up later.
        }
        callbacks.current.onReady?.({
          write: (data) => term.write(data),
          focus: () => term.focus(),
          size: () => size,
        })
        term.focus()
      })
      .catch((err: unknown) => {
        if (disposed) return
        // Surface init failures (e.g. WASM blocked by CSP) inside the terminal area itself.
        container.textContent = `Terminal failed to initialize: ${
          err instanceof Error ? err.message : String(err)
        }`
      })

    return () => {
      disposed = true
      term.destroy()
    }
  }, [])

  return (
    <div
      ref={containerRef}
      data-testid={testId}
      aria-label="SSH terminal"
      className={`keel-web-terminal ${className ?? ""}`.trim()}
    />
  )
}
