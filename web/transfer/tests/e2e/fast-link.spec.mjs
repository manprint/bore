// T-FL-PW: a real browser downloads a fast link once (plan 004, 2.3).
//
// The uploader is real `curl -T`, the server is a real `bore server`
// configured only through the BORE_FAST_LINK_TRANSFER_* environment, and the
// downloader is the browser following the printed link: the reference
// scenario end to end. Nothing here depends on the web-transfer page.
import { test, expect } from "@playwright/test";
import { spawn, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { boreBin, freePort, waitPort } from "./helpers.mjs";

// The fast host must resolve to loopback inside the browser, and the test
// certificate is self-signed; both are properties of this harness, not of
// the behaviour under test (standard HTTP attachment handling).
test.use({
  ignoreHTTPSErrors: true,
  launchOptions: {
    args: ["--host-resolver-rules=MAP fast.bore.local 127.0.0.1"],
  },
});
test.skip(
  ({ browserName }) => browserName !== "chromium",
  "--host-resolver-rules is a Chromium switch; the behaviour under test is standard HTTP download handling",
);

const PASS = `pw-${randomBytes(6).toString("hex")}`;
let dir = null;
let server = null;
let uploader = null;

test.afterEach(() => {
  for (const child of [uploader, server]) {
    if (child && child.exitCode === null) {
      child.kill("SIGKILL");
    }
  }
  uploader = null;
  server = null;
  if (dir) {
    rmSync(dir, { recursive: true, force: true });
    dir = null;
  }
});

/** Resolve with the first `https://` line the uploader prints. */
function firstLink(child) {
  return new Promise((resolve, reject) => {
    let seen = "";
    const onData = (chunk) => {
      seen += chunk.toString("utf8");
      const line = seen.split("\n").find((l) => l.startsWith("https://"));
      if (line) {
        child.stdout.off("data", onData);
        resolve(line.trim());
      }
    };
    child.stdout.on("data", onData);
    child.once("exit", (code) =>
      reject(new Error(`uploader exited ${code} before printing a link: ${seen}`)),
    );
  });
}

test("a browser downloads a fast link once", async ({ page }) => {
  dir = mkdtempSync(join(tmpdir(), "bore-fast-link-pw-"));
  const cert = join(dir, "cert.pem");
  const key = join(dir, "key.pem");
  const gen = spawnSync(
    "openssl",
    [
      "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
      "-keyout", key, "-out", cert, "-subj", "/CN=bore.local",
      "-addext", "subjectAltName=DNS:bore.local,DNS:*.bore.local",
    ],
    { stdio: "ignore" },
  );
  expect(gen.status, "openssl must create the test certificate").toBe(0);

  const payload = randomBytes(32 * 1024 * 1024);
  const payloadHash = createHash("sha256").update(payload).digest("hex");
  const payloadPath = join(dir, "payload.bin");
  writeFileSync(payloadPath, payload);

  const [cp, hp, sp] = [await freePort(), await freePort(), await freePort()];
  server = spawn(
    boreBin,
    [
      "server", "--bind-addr", "127.0.0.1", "--bind-tunnels", "127.0.0.1",
      "--control-port", String(cp), "--vhost-base-domain", "bore.local",
      "--vhost-mode", "both", "--vhost-http-port", String(hp),
      "--vhost-https-port", String(sp), "--vhost-cert-file", cert,
      "--vhost-key-file", key,
    ],
    {
      env: {
        ...process.env,
        BORE_FAST_LINK_TRANSFER_ENABLED: "true",
        BORE_FAST_LINK_TRANSFER_VHOST: "fast.bore.local",
        BORE_FAST_LINK_TRANSFER_AUTH: `u:${PASS}`,
      },
      stdio: "ignore",
    },
  );
  await waitPort(sp);

  uploader = spawn(
    "curl",
    [
      "-sS", "-N", "-u", `u:${PASS}`, "--cacert", cert,
      "--resolve", `fast.bore.local:${sp}:127.0.0.1`,
      "-T", payloadPath, `https://fast.bore.local:${sp}/payload.bin`,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  const uploaderExit = new Promise((resolve) => uploader.once("exit", resolve));
  const link = await firstLink(uploader);
  expect(link).toMatch(
    new RegExp(`^https://fast\\.bore\\.local:${sp}/[a-z0-9]{16}/payload\\.bin$`),
  );

  // Navigating to an attachment starts a download and rejects the
  // navigation itself ("Download is starting"): that rejection is expected.
  const downloadEvent = page.waitForEvent("download");
  await page.goto(link).catch(() => {});
  const download = await downloadEvent;
  expect(download.suggestedFilename()).toBe("payload.bin");
  const saved = await download.path();
  const savedHash = createHash("sha256").update(readFileSync(saved)).digest("hex");
  expect(savedHash).toBe(payloadHash);

  expect(await uploaderExit, "the uploader must see a complete transfer").toBe(0);

  // One download per link: the same URL now answers 404 (and is a page,
  // not an attachment, so the navigation resolves normally).
  const again = await page.goto(link);
  expect(again?.status()).toBe(404);
});
