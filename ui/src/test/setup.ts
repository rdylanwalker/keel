import "@testing-library/jest-dom/vitest"

// jsdom lacks ResizeObserver, which Radix primitives (Checkbox, Tooltip) use for sizing.
if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver
}
