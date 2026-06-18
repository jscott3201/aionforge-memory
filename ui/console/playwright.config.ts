import { defineConfig, devices } from "@playwright/test";

const port = Number(process.env.AIONFORGE_CONSOLE_E2E_PORT ?? 4173);
const baseURL =
  process.env.AIONFORGE_CONSOLE_E2E_BASE_URL ?? `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: "tests/e2e",
  fullyParallel: true,
  timeout: 30_000,
  expect: {
    timeout: 5_000,
  },
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL,
    screenshot: "only-on-failure",
    trace: "on-first-retry",
  },
  webServer: {
    command: `pnpm preview -- --port ${port}`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    url: `${baseURL}/console`,
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
    {
      name: "mobile-chromium",
      use: { ...devices["Pixel 7"] },
    },
  ],
});
