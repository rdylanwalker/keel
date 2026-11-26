import { defineConfig, devices } from '@playwright/test';

const isLive = process.env.KEEL_E2E_LIVE === '1';
const baseURL =
  process.env.KEEL_BASE_URL || (isLive ? 'http://127.0.0.1:8080' : 'http://127.0.0.1:18080');

export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: isLive ? 0 : process.env.CI ? 2 : 0,
  workers: 1,
  timeout: isLive ? 20 * 60 * 1000 : 30 * 1000,
  reporter: isLive ? [['list'], ['html', { open: 'never' }]] : 'html',
  ...(isLive
    ? {}
    : {
        webServer: {
          command: 'node ./scripts/start-test-server.mjs',
          url: `${baseURL}/healthz`,
          reuseExistingServer: !process.env.CI,
          // Covers the incremental `cargo build` in start-test-server.mjs after code changes.
          timeout: 10 * 60 * 1000,
        },
      }),
  use: {
    baseURL,
    ignoreHTTPSErrors: true,
    // Bounded per-action/navigation timeouts: a stuck locator fails fast with a useful error
    // instead of silently consuming the whole (20 min live) test timeout.
    actionTimeout: isLive ? 30_000 : 10_000,
    navigationTimeout: isLive ? 60_000 : 15_000,
    trace: isLive ? 'retain-on-failure' : 'on-first-retry',
    screenshot: 'only-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'firefox',
      use: { ...devices['Desktop Firefox'] },
    },
  ],
});
