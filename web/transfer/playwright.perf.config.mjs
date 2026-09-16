import { defineConfig, devices } from "@playwright/test";

// The benchmark configuration is deliberately NOT the test one: measurements
// must not run in parallel (two transfers would measure each other's
// contention), and the default engine set is one, because an arm per engine
// is a comparison and the driver asks for it explicitly.
export default defineConfig({
  testDir: "./tests/perf",
  testMatch: "**/*.perf.mjs",
  fullyParallel: false,
  workers: 1,
  reporter: "list",
  timeout: 10 * 60 * 1000,
  use: { trace: "off" },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
