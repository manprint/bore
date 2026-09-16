import { defineConfig, devices } from "@playwright/test";

// Default run covers the three open engines. Branded Chrome/Edge are optional
// named projects (release smoke only), excluded via the --project selection in
// the test:e2e script, never by deleting them here.
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: true,
  reporter: "list",
  use: {
    // Failure-only artifacts, and VIDEO ONLY.
    //
    // A recording of a passing run is a copy of a room's page for no reason;
    // on a failure it is the only way to see what a headless engine did. It
    // is safe to keep because the app scrubs the room fragment out of the
    // address bar as soon as it has read it (`src/secrets.js`), so the
    // recording never shows the member token or the room key.
    //
    // Traces and screenshots are OFF, and that is a MEASUREMENT rather than a
    // preference: both INSTRUMENT the page — trace snapshots inject their
    // own markup, and a WebKit screenshot injects a caret-hiding stylesheet —
    // and this page is served under `style-src 'self'` with no
    // `unsafe-inline`. WebKit refuses the injected stylesheet and logs it,
    // and the specs treat a console error as a failure, so either option
    // makes `three peers appear, rename and leave` fail deterministically on
    // the CSP doing exactly its job. Verified one at a time: all three off
    // passes, video alone passes, screenshot alone fails. A debugging aid
    // must not change what it is observing.
    trace: "off",
    screenshot: "off",
    video: "retain-on-failure",
  },
  // Artifacts are named after the TEST, never after a room URL: a directory
  // name ends up in a CI artifact listing, which is public on this repo.
  outputDir: "test-results",
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
