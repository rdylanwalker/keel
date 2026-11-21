import { describe, expect, it } from "vitest"
import { formatLastSynced } from "./format"

describe("formatLastSynced", () => {
  const now = Date.parse("2026-01-10T12:00:00Z")
  it("formats relative ages", () => {
    expect(formatLastSynced(undefined, now)).toBe("—")
    expect(formatLastSynced("  ", now)).toBe("—")
    expect(formatLastSynced("yesterday-ish", now)).toBe("yesterday-ish")
    expect(formatLastSynced("2026-01-10T11:59:30Z", now)).toBe("just now")
    expect(formatLastSynced("2026-01-10T11:55:00Z", now)).toBe("5m ago")
    expect(formatLastSynced("2026-01-10T09:00:00Z", now)).toBe("3h ago")
    expect(formatLastSynced("2026-01-08T12:00:00Z", now)).toBe("2d ago")
  })
})
