// Deterministic bundler for the web-transfer browser shell.
// Entry: src/main.js (imports styles.css) -> <outdir>/app.js + <outdir>/app.css.
// Worker: src/offer-worker.js -> <outdir>/offer-worker.js as ESM (loaded by
// the app with `new Worker(..., { type: "module" })`).
// index.html is copied verbatim. No content hashes, no source maps: release
// dist must be byte-reproducible (see tests/unit/scaffold.test.mjs).
// WT_OUTDIR overrides the output directory (used by the reproducibility test).
import { cpSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { buildSync } from "esbuild";

const root = dirname(fileURLToPath(import.meta.url));
const outdir = process.env.WT_OUTDIR ?? join(root, "dist");

mkdirSync(outdir, { recursive: true });

const shared = {
  outdir,
  bundle: true,
  target: ["chrome120", "edge120", "firefox120", "safari17"],
  minify: true,
  sourcemap: false,
  metafile: false,
  logLevel: "warning",
};

buildSync({
  ...shared,
  entryPoints: { app: join(root, "src", "main.js") },
  format: "iife",
  platform: "browser",
});

buildSync({
  ...shared,
  entryPoints: { "offer-worker": join(root, "src", "offer-worker.js") },
  format: "esm",
  platform: "browser",
});

// index.html references the fixed asset names; copy it unchanged.
cpSync(join(root, "src", "index.html"), join(outdir, "index.html"));
