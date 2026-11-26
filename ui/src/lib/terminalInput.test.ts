import { describe, expect, it } from "vitest"
import { TERMINAL_INPUT_CHUNK, chunkTerminalInput } from "./terminalInput"

describe("chunkTerminalInput", () => {
  it("passes keystrokes through as a single frame", () => {
    expect(chunkTerminalInput("l")).toEqual(["l"])
    expect(chunkTerminalInput("")).toEqual([])
  })

  it("splits large pastes into frames that stay under the server's 64 KiB limit", () => {
    const paste = "\u0001".repeat(TERMINAL_INPUT_CHUNK * 3 + 7)
    const chunks = chunkTerminalInput(paste)
    expect(chunks.map((c) => c.length)).toEqual([
      TERMINAL_INPUT_CHUNK,
      TERMINAL_INPUT_CHUNK,
      TERMINAL_INPUT_CHUNK,
      7,
    ])
    expect(chunks.join("")).toBe(paste)
    for (const chunk of chunks) {
      const frame = JSON.stringify({ type: "input", data: chunk })
      expect(new TextEncoder().encode(frame).length).toBeLessThan(64 * 1024)
    }
  })

  it("never splits a surrogate pair", () => {
    const paste = "ab🚀cd🚀"
    const chunks = chunkTerminalInput(paste, 3)
    expect(chunks.join("")).toBe(paste)
    expect(chunks).toEqual(["ab", "🚀c", "d🚀"])
    for (const chunk of chunks) {
      // Throws URIError on a lone surrogate.
      expect(() => encodeURIComponent(chunk)).not.toThrow()
    }
  })
})
