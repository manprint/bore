// Deterministic bundler for the web-transfer browser shell.
// Entry: src/main.js (imports styles.css) -> <outdir>/app.js + <outdir>/app.css.
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

buildSync({
  entryPoints: { app: join(root, "src", "main.js") },
  outdir,
  bundle: true,
  format: "iife",
  platform: "browser",
  target: ["chrome120", "edge120", "firefox120", "safari17"],
  minify: true,
  sourcemap: false,
  metafile: false,
  logLevel: "warning",
});

// index.html references the fixed asset names; copy it unchanged.
cpSync(join(root, "src", "index.html"), join(outdir, "index.html"));
