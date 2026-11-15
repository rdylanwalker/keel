import { defineConfig, devices } from '@playwright/test';

const baseURL = process.env.KEEL_BASE_URL || 'http://127.0.0.1:18080';

export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1,
  reporter: 'html',
  webServer: {
    command: 'node ./scripts/start-test-server.mjs',
    url: `${baseURL}/healthz`,
    reuseExistingServer: !process.env.CI,
    timeout: 60000,
  },
  use: {
    baseURL,
    trace: 'on-first-retry',
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
