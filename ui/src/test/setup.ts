import "@testing-library/jest-dom/vitest"
import { loadDemoFixtures } from "../lib/demo"

// Components read demo fixtures synchronously in `?demo=true` mode; main.tsx preloads them.
await loadDemoFixtures()

// jsdom lacks ResizeObserver, which Radix primitives (Checkbox) use for sizing.
if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
}
