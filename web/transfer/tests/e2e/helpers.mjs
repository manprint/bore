// Shared Playwright setup for the web-transfer room suites: a real `bore
// server` with web-transfer on a dynamic loopback port plus one room owner
// holding its lease. Both binaries come from a prior
// `cargo test --all-features` / `cargo build --all-features`.
import { spawn } from "node:child_process";
import net from "node:net";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createHash, hkdfSync } from "node:crypto";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, firefox, webkit } from "@playwright/test";
import {
  disableWebRtc,
  forceIceRelayOnly,
  installTestHooks,
} from "./fixtures.js";

/** Persistent-profile launchers by Playwright project browser name. */
const PERSISTENT_ENGINES = { chromium, firefox, webkit };

export const root = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "..",
);
// The binaries under test. `debug` by default, because that is what a
// developer has just built; `BORE_E2E_BIN` / `BORE_E2E_OWNER_BIN` point the
// whole suite at the RELEASE build instead, which is what the deployment
// gates run — the acceptance claim is about the artefact that ships, and a
// debug binary is not it.
export const boreBin =
  process.env.BORE_E2E_BIN ?? join(root, "target", "debug", "bore");
export const ownerBin =
  process.env.BORE_E2E_OWNER_BIN ??
  join(root, "target", "debug", "examples", "web_transfer_e2e_owner");

const ROOM_LINK_SEED_TEXT_BYTES = 22;
const ROOM_LINK_SALT = Buffer.from("bore-web-transfer-link-v1");
const ROOM_LINK_INFO = {
  roomId: Buffer.from("bore-web-transfer-room-id-v1"),
  memberToken: Buffer.from("bore-web-transfer-member-token-v1"),
  roomKey: Buffer.from("bore-web-transfer-room-key-v1"),
};

function decodeRoomLinkSeed(seedText) {
  if (!/^[A-Za-z0-9_-]{22}$/.test(seedText)) {
    throw new Error("invalid short room-link seed");
  }
  const seed = Buffer.from(seedText, "base64url");
  if (
    seed.length !== 16 ||
    seed.toString("base64url") !== seedText
  ) {
    throw new Error("invalid short room-link seed");
  }
  return seed;
}

/** Independent Node oracle for the browser's three HKDF calls. */
export function deriveShortLinkMaterial(roomUrl) {
  let parsed;
  try {
    parsed = new URL(roomUrl);
  } catch {
    throw new Error("invalid short room URL");
  }
  const seedText = parsed.hash.startsWith("#") ? parsed.hash.slice(1) : "";
  if (
    parsed.pathname !== "/transfer/" ||
    parsed.search !== "" ||
    seedText.length !== ROOM_LINK_SEED_TEXT_BYTES
  ) {
    throw new Error("invalid short room URL");
  }
  const seed = decodeRoomLinkSeed(seedText);
  const derive = (info) =>
    Buffer.from(hkdfSync("sha256", seed, ROOM_LINK_SALT, info, 32));
  return {
    seedText,
    roomId: derive(ROOM_LINK_INFO.roomId).subarray(0, 16).toString("hex"),
    memberToken: derive(ROOM_LINK_INFO.memberToken).toString("hex"),
    roomKey: derive(ROOM_LINK_INFO.roomKey).toString("hex"),
  };
}

export function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
}

export async function waitPort(target, retries = 100) {
  for (let i = 0; i < retries; i += 1) {
    const open = await new Promise((resolve) => {
      const socket = net.connect(target, "127.0.0.1");
      socket.once("connect", () => {
        socket.end();
        resolve(true);
      });
      socket.once("error", () => resolve(false));
    });
    if (open) {
      return;
    }
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`port ${target} never opened`);
}

/**
 * Starts a server plus one lease-holding owner. Returns
 * `{ port, roomUrl, roomId, memberToken, roomKey, cleanup }`.
 *
 * `relayRate` (bytes/s) throttles the room's relay, which is what makes a
 * mid-transfer cancel observable without a huge file: the pace is the
 * server's, so the leg does not depend on how fast this machine is.
 */
/**
 * A room that ALREADY exists, created by something outside this process.
 *
 * `spawnRoomEnv` starts its own server, which is the right default for every
 * ordinary spec. The deployment gates are different: the server under test is
 * a container, or a release binary behind a proxy, and the point is precisely
 * that this process did not build it. The room URL then arrives through
 * `BORE_WEB_E2E_ROOM_URL` and the caller owns the server's life.
 *
 * Returns `null` when the variable is unset, so a spec can skip rather than
 * invent a server and quietly test something else.
 */
export function externalRoomEnv() {
  const roomUrl = process.env.BORE_WEB_E2E_ROOM_URL;
  if (!roomUrl) {
    return null;
  }
  let parsed;
  try {
    parsed = new URL(roomUrl);
  } catch {
    throw new Error("invalid short room URL");
  }
  const material = deriveShortLinkMaterial(roomUrl);
  return {
    roomUrl,
    port: Number(parsed.port),
    ...material,
    cleanup: () => {},
  };
}

/**
 * Every child this module starts, so a run that ends WITHOUT reaching its own
 * teardown — a spec that throws, a worker the runner ends after a timeout, a
 * Ctrl+C — does not leave a server and a room owner behind. MEASURED on the
 * dev box: 39 orphaned processes, the oldest alive for 24 hours, each holding
 * a control port and a room. The registry is the structural half; a `cleanup()`
 * a spec forgets to call is the half that has to be remembered.
 *
 * `exit` is the one event that fires for a normal end, an uncaught exception
 * and a signal the runner handles, and it cannot await — so the kill is
 * synchronous. A SIGKILL of the runner itself cannot be covered by anything
 * inside it.
 */
const spawnedChildren = new Set();

function track(child) {
  spawnedChildren.add(child);
  child.on("exit", () => spawnedChildren.delete(child));
  return child;
}

process.on("exit", () => {
  for (const child of spawnedChildren) {
    try {
      child.kill("SIGKILL");
    } catch {
      // Already gone: reaping twice is not an error.
    }
  }
  spawnedChildren.clear();
});

export async function spawnRoomEnv({
  relayRate,
  ownerGrace,
  noStun = false,
  relayOnly = false,
} = {}) {
  const port = await freePort();
  const server = track(
    spawn(
      boreBin,
      [
        "server",
        "--control-port",
        String(port),
        "--web-transfer-base-url",
        `http://127.0.0.1:${port}/`,
        ...(relayRate === undefined
          ? []
          : ["--web-transfer-relay-rate", String(relayRate)]),
        // The security suite needs the room to die WHILE a relay is running;
        // with the shipped 60 s grace that case cannot be observed at all.
        ...(ownerGrace === undefined
          ? []
          : ["--web-transfer-owner-grace", String(ownerGrace)]),
        // Host candidates only. On a loopback pair a reflexive address buys
        // nothing, and the public STUN chain is a real network dependency
        // inside a measurement: the benchmark asks for it explicitly.
        ...(noStun ? ["--web-transfer-no-stun"] : []),
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    ),
  );
  server.on("error", (error) => {
    throw new Error(
      `cannot spawn ${boreBin}: ${error.message} (run cargo build --all-features first)`,
    );
  });
  if (process.env.BORE_E2E_LOG) {
    // Opt-in: the server's own view of a run, for diagnosing a failure the
    // page cannot see. Off by default so a green run stays quiet.
    server.stderr.on("data", (chunk) =>
      process.stderr.write(`[server] ${chunk}`),
    );
    server.stdout.on("data", (chunk) =>
      process.stderr.write(`[server] ${chunk}`),
    );
  }
  await waitPort(port);
  // Freshness: the server embeds dist at COMPILE time, so `npm run build`
  // alone changes nothing the suite can see. This used to look for one
  // string inside `app.js`, which answers "is there a bundle at all" and not
  // "is it the one on disk" — and it is blind to `app.css` entirely, so a
  // stylesheet gate ran against whatever CSS the binary was built with. The
  // check is now a hash comparison, per asset, and it names the file: a
  // stale stylesheet and a stale script need the same command but produce
  // very different confusion.
  for (const name of ["app.js", "app.css"]) {
    const served = await fetch(`http://127.0.0.1:${port}/transfer/assets/${name}`, {
      headers: { Host: `127.0.0.1:${port}` },
    }).then((response) => response.arrayBuffer());
    const onDisk = readFileSync(join(root, "web", "transfer", "dist", name));
    const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
    if (digest(Buffer.from(served)) !== digest(onDisk)) {
      server.kill("SIGKILL");
      throw new Error(
        `stale embedded ${name}: the server is serving a different build than web/transfer/dist/${name}. ` +
          "Run `npm run build --prefix web/transfer` then `cargo build --all-features`.",
      );
    }
  }
  const owner = track(
    spawn(ownerBin, [`127.0.0.1:${port}`, ...(relayOnly ? ["relay-only"] : [])], {
      stdio: ["ignore", "pipe", "pipe"],
    }),
  );
  owner.on("error", (error) => {
    throw new Error(
      `cannot spawn ${ownerBin}: ${error.message} (run cargo build --all-features first)`,
    );
  });
  let buffered = "";
  const roomUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("owner printed no room URL in time")),
      30_000,
    );
    owner.stdout.on("data", (chunk) => {
      buffered += String(chunk);
      const line = buffered
        .split("\n")
        .find((candidate) => candidate.startsWith("WEB_TRANSFER_ROOM_URL="));
      if (line !== undefined) {
        clearTimeout(timer);
        resolve(line.slice("WEB_TRANSFER_ROOM_URL=".length).trim());
      }
    });
    owner.stdout.on("error", reject);
  });
  let material;
  try {
    material = deriveShortLinkMaterial(roomUrl);
  } catch {
    server.kill("SIGKILL");
    owner.kill("SIGKILL");
    throw new Error("unparseable short room URL");
  }
  return {
    port,
    roomUrl,
    ...material,
    // Kills ONLY the lease holder: the server keeps running, so the room's
    // own expiry is what the test observes.
    killOwner: () => owner.kill("SIGKILL"),
    cleanup: () => {
      owner.kill("SIGKILL");
      server.kill("SIGKILL");
    },
  };
}

/** Canonical JSON (sorted keys, no whitespace) for the MAC cross-check. */
export function canonicalize(value) {
  if (value === null) return "null";
  if (value === true) return "true";
  if (value === false) return "false";
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) {
      throw new Error("no canonical form");
    }
    return String(value);
  }
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) {
    return `[${value.map(canonicalize).join(",")}]`;
  }
  if (typeof value === "object") {
    const keys = Object.keys(value).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalize(value[k])}`).join(",")}}`;
  }
  throw new Error("no canonical form");
}

/**
 * A peer running in a browser context with a real on-disk profile.
 *
 * OPFS needs one. In Playwright's default (ephemeral) WebKit context
 * `navigator.storage.getDirectory` exists as a function and then rejects with
 * `UnknownError`, so a download leg run there reports the whole feature as
 * unsupported instead of exercising it; with a persistent profile the same
 * call succeeds on all three engines (V002-F02). Every engine gets the same
 * treatment so the legs stay identical.
 *
 * @param {string} url room URL
 * @param {object} options `{ browserName, channel, init }` — `init` is an
 * init script evaluated before the app boots
 * @returns `{ context, page, failures, cleanup }`
 */
export async function openPersistentPeer(
  url,
  {
    browserName,
    channel,
    init,
    noWebRtc = false,
    iceRelayOnly = false,
    // Off by default, and it stays off for every gate: a suite that ignores
    // certificate errors cannot notice a broken one. The two-host perf leg
    // turns it on because it serves a self-signed certificate for a LAN
    // address on purpose (T-WEB-PERF-LAN).
    ignoreHttpsErrors = false,
  } = {},
) {
  const engine = PERSISTENT_ENGINES[browserName];
  if (engine === undefined) {
    throw new Error(`no persistent launcher for browser ${browserName}`);
  }
  const profile = mkdtempSync(join(tmpdir(), "bore-profile-"));
  const context = await engine.launchPersistentContext(profile, {
    acceptDownloads: true,
    ...(ignoreHttpsErrors ? { ignoreHTTPSErrors: true } : {}),
    ...(channel === undefined ? {} : { channel }),
  });
  await installTestHooks(context);
  if (noWebRtc) {
    await disableWebRtc(context);
  }
  if (iceRelayOnly) {
    await forceIceRelayOnly(context);
  }
  if (init) {
    await context.addInitScript(init);
  }
  const page = context.pages()[0] ?? (await context.newPage());
  const failures = [];
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error") {
      failures.push(`console: ${message.text()}`);
    }
  });
  await page.goto(url);
  return {
    context,
    page,
    failures,
    cleanup: async () => {
      await context.close().catch(() => {});
      rmSync(profile, { recursive: true, force: true });
    },
  };
}

/** True when this context can really open OPFS (not merely expose it). */
export async function opfsWorks(page) {
  return page.evaluate(() =>
    navigator.storage?.getDirectory === undefined
      ? false
      : navigator.storage.getDirectory().then(
          () => true,
          () => false,
        ),
  );
}
