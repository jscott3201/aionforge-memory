import { defineConfig, devices } from "@playwright/test";

const port = Number(process.env.AIONFORGE_CONSOLE_E2E_PORT ?? 4183);
const baseURL =
  process.env.AIONFORGE_CONSOLE_E2E_BASE_URL ?? `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: "tests/e2e",
  testMatch: "live-data.spec.ts",
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  expect: {
    timeout: 10_000,
  },
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL,
    screenshot: "only-on-failure",
    trace: "on-first-retry",
  },
  webServer: {
    command: "node tests/e2e/start-live-server.mjs",
    reuseExistingServer: false,
    timeout: 180_000,
    url: `${baseURL}/console`,
  },
  projects: [
    {
      name: "live-chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
