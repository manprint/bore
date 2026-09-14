import { defineConfig, devices } from "@playwright/test";

// Default run covers the three open engines. Branded Chrome/Edge are optional
// named projects (release smoke only), excluded via the --project selection in
// the test:e2e script, never by deleting them here.
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: true,
  reporter: "list",
  use: {
    trace: "off",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
    {
      name: "branded-chrome",
      use: { ...devices["Desktop Chrome"], channel: "chrome" },
    },
    {
      name: "branded-edge",
      use: { ...devices["Desktop Chrome"], channel: "msedge" },
    },
  ],
});
