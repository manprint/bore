// Scaffold unit tests (run by `npm run check` via `node --test tests/unit/`).
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  cpSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  symlinkSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");
const srcHtml = join(root, "src", "index.html");
const distDir = join(root, "dist");
const distHtml = join(distDir, "index.html");

function readKind(path) {
  return readFileSync(path, "utf8");
}

function assertNoInlineExecutableContent(html, where) {
  // Inline <script> without src (executable page content); external bundle
  // scripts must carry src and defer.
  for (const match of html.matchAll(/<script\b[^>]*>/gi)) {
    assert.match(
      match[0],
      /\ssrc\s*=/i,
      `${where}: inline <script> without src: ${match[0]}`,
    );
  }
  assert.doesNotMatch(html, /<style\b/i, `${where}: inline <style> block`);
  assert.doesNotMatch(
    html,
    /\son[a-z]+\s*=/i,
    `${where}: inline event-handler attribute`,
  );
  assert.doesNotMatch(
    html,
    /javascript\s*:/i,
    `${where}: javascript: URL scheme`,
  );
}

function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function dirSnapshot(dir) {
  const out = new Map();
  for (const name of readdirSync(dir).sort()) {
    out.set(name, sha256File(join(dir, name)));
  }
  return out;
}

describe("web-transfer scaffold", () => {
  it("scaffold_has_no_inline_executable_content", () => {
    assertNoInlineExecutableContent(readKind(srcHtml), "src/index.html");
    assertNoInlineExecutableContent(readKind(distHtml), "dist/index.html");
    const dist = readKind(distHtml);
    assert.match(
      dist,
      /<script\s+src="\/transfer\/assets\/app\.js"\s+defer><\/script>/,
      "dist shell loads only the bundled app.js",
    );
    assert.match(
      dist,
      /<link\s+rel="stylesheet"\s+href="\/transfer\/assets\/app\.css">/,
      "dist shell loads only the bundled app.css",
    );
  });

  it("build_is_reproducible", () => {
    const tmp = mkdtempSync(join(tmpdir(), "bore-wt-scaffold-"));
    // Temporary COPY of build inputs: the test must not depend on cwd state.
    cpSync(join(root, "src"), join(tmp, "src"), { recursive: true });
    cpSync(join(root, "esbuild.mjs"), join(tmp, "esbuild.mjs"));
    // ESM ignores NODE_PATH: link the real node_modules so the copied
    // bundler script resolves `esbuild` exactly like the working tree does.
    symlinkSync(join(root, "node_modules"), join(tmp, "node_modules"), "dir");
    const env = { ...process.env };
    const outs = [join(tmp, "out1"), join(tmp, "out2")];
    for (const out of outs) {
      execFileSync(process.execPath, [join(tmp, "esbuild.mjs")], {
        env: { ...env, WT_OUTDIR: out },
        stdio: "pipe",
      });
    }
    const [first, second] = outs.map(dirSnapshot);
    assert.deepEqual(
      [...first.entries()],
      [...second.entries()],
      "two builds from one copy must agree file-for-file",
    );
    const committed = dirSnapshot(distDir);
    assert.deepEqual(
      [...first.entries()],
      [...committed.entries()],
      "committed dist must match a fresh build (run npm run build)",
    );
  });
});
