import type * as DemoFixtureModule from "./demoFixtures"

export type DemoFixtures = typeof DemoFixtureModule

let loaded: DemoFixtures | null = null

/**
 * Loads the bundled demo fixtures (UI-9). The gate reads `import.meta.env` directly so Vite
 * constant-folds it: builds without `VITE_ENABLE_DEMO=true` drop the dynamic import and never
 * emit the fixture chunk. Call before rendering when {@link isDemoMode} is active.
 */
export async function loadDemoFixtures(): Promise<void> {
  if (import.meta.env.DEV || import.meta.env.VITE_ENABLE_DEMO === "true") {
    loaded = await import("./demoFixtures")
  }
}

/** The loaded demo fixtures; only valid in demo mode after {@link loadDemoFixtures}. */
export function demoFixtures(): DemoFixtures {
  if (!loaded) {
    throw new Error("Demo fixtures are not loaded")
  }
  return loaded
}
