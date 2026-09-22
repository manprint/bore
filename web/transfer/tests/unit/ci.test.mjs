// CI-contract unit tests (6.4): the pieces a browser matrix rests on, checked
// as DATA rather than trusted to a workflow file nobody reads on a red run.
//
// Three claims:
//   1. the Playwright config still declares the three engines the
//      compatibility claim is made about, plus the branded channels as
//      OPT-IN projects, and keeps artifacts failure-only;
//   2. the lockfile pins exactly the versions `package.json` names, so `npm
//      ci` in CI installs what a workstation installed;
//   3. the e2e script selects the three engines explicitly — a matrix that
//      silently loses an engine still goes green.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");

describe("ci contract", () => {
  it("playwright_config_defines_required_projects_and_secret_safe_artifacts", () => {
    const config = readFileSync(join(root, "playwright.config.mjs"), "utf8");
    for (const project of ["chromium", "firefox", "webkit"]) {
      assert.ok(
        config.includes(`name: "${project}"`),
        `the engine matrix lost ${project}`,
      );
    }
    for (const branded of ["branded-chrome", "branded-edge"]) {
      assert.ok(config.includes(`name: "${branded}"`), `${branded} is not declared`);
    }
    // Artifacts only on failure: a passing run leaves no copy of a room.
    assert.match(config, /video:\s*"retain-on-failure"/);
    // Traces and screenshots stay OFF: both instrument the page, and this
    // page's CSP refuses what they inject — turning either on makes a WebKit
    // spec fail on the policy working. Measured one at a time, not assumed.
    assert.match(config, /trace:\s*"off"/);
    assert.match(config, /screenshot:\s*"off"/);

    const esbuild = readFileSync(join(root, "esbuild.mjs"), "utf8");
    assert.match(
      esbuild,
      /target:\s*\["chrome120",\s*"edge120",\s*"firefox120",\s*"safari17"\]/,
      "the bundle target matrix drifted from the supported engines",
    );

    const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
    // The default e2e run names its engines; a project dropped from the
    // config would otherwise just stop running, silently and green.
    for (const project of ["chromium", "firefox", "webkit"]) {
      assert.ok(
        pkg.scripts["test:e2e"].includes(`--project=${project}`),
        `test:e2e does not run ${project}`,
      );
    }
    // And it does NOT name the branded channels: they are not installed on
    // every machine, and a run that skips them must say so rather than pass.
    assert.ok(!pkg.scripts["test:e2e"].includes("branded"));
    assert.ok(
      pkg.scripts["test:e2e:branded"].includes("--project=branded-chrome") &&
        pkg.scripts["test:e2e:branded"].includes("--project=branded-edge"),
      "the branded smoke has no script of its own",
    );
  });

  it("package_lock_versions_equal_plan_pins", () => {
    const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
    const lock = JSON.parse(readFileSync(join(root, "package-lock.json"), "utf8"));
    assert.equal(lock.lockfileVersion, 3, "npm ci expects a v3 lockfile");
    const declared = { ...pkg.dependencies, ...pkg.devDependencies };
    for (const [name, version] of Object.entries(declared)) {
      // Exact pins only: a range in this file is a different install on
      // every machine, and the browser matrix is the one place that costs
      // hours to debug.
      assert.match(
        version,
        /^\d+\.\d+\.\d+$/,
        `${name} is pinned as ${version}, not an exact version`,
      );
      const entry = lock.packages[`node_modules/${name}`];
      assert.ok(entry !== undefined, `${name} is missing from the lockfile`);
      assert.equal(entry.version, version, `${name} drifted from its pin`);
    }
    assert.equal(lock.packages[""].name, pkg.name);
  });
});
