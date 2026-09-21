// T-WEB-BROWSER-ROOM: the committed app against a real server room.
//
// Setup (beforeAll): a real `bore server` with web-transfer on a dynamic
// loopback port, plus one room owner (examples/web_transfer_e2e_owner,
// holding its lease for the whole file). Both binaries come from a prior
// `cargo test --all-features` / `cargo build --all-features` — the spec
// fails fast with that hint when they are missing or stale.
import { spawn } from "node:child_process";
import net from "node:net";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "@playwright/test";
import { deriveShortLinkMaterial } from "./helpers.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "..");
const boreBin = join(root, "target", "debug", "bore");
const ownerBin = join(root, "target", "debug", "examples", "web_transfer_e2e_owner");

let server;
let owner;
let port;
let roomUrl;
let roomId;
let memberToken;
let roomSeedText;

function freePort() {
  return new Promise((resolve, reject) => {
    const probe = net.createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const { port: picked } = probe.address();
      probe.close(() => resolve(picked));
    });
  });
}

async function waitPort(target, retries = 100) {
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

test.beforeAll(async () => {
  port = await freePort();
  const baseUrl = `http://127.0.0.1:${port}/`;
  server = spawn(boreBin, ["server", "--control-port", String(port), "--web-transfer-base-url", baseUrl], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  server.on("error", (error) => {
    throw new Error(`cannot spawn ${boreBin}: ${error.message} (run cargo build --all-features first)`);
  });
  await waitPort(port);
  // Freshness: the server embeds dist at compile time, so a binary older
  // than the last frontend build serves a stale shell with no room logic.
  // Fail fast with the rebuild hint instead of timing out on connections
  // that can never complete.
  const asset = await fetch(`http://127.0.0.1:${port}/transfer/assets/app.js`, {
    headers: { Host: `127.0.0.1:${port}` },
  }).then((response) => response.text());
  if (!asset.includes("peer-list")) {
    throw new Error(
      "stale embedded app bundle: run cargo build --all-features after npm run build",
    );
  }
  owner = spawn(ownerBin, [`127.0.0.1:${port}`], { stdio: ["ignore", "pipe", "pipe"] });
  owner.on("error", (error) => {
    throw new Error(`cannot spawn ${ownerBin}: ${error.message} (run cargo build --all-features first)`);
  });
  // Accumulate stdout until the URL line arrives (chunking-safe).
  let buffered = "";
  roomUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("owner printed no room URL in time")), 30_000);
    owner.stdout.on("data", (chunk) => {
      buffered += String(chunk);
      const line = buffered.split("\n").find((candidate) => candidate.startsWith("WEB_TRANSFER_ROOM_URL="));
      if (line !== undefined) {
        clearTimeout(timer);
        resolve(line.slice("WEB_TRANSFER_ROOM_URL=".length).trim());
      }
    });
    owner.stdout.on("error", reject);
  });
  const material = deriveShortLinkMaterial(roomUrl);
  roomId = material.roomId;
  memberToken = material.memberToken;
  roomSeedText = material.seedText;
}, 60_000);

test.afterAll(async () => {
  owner?.kill("SIGKILL");
  server?.kill("SIGKILL");
});

// One instrumented context: RTC construction counting, clipboard capture,
// websocket frame audit. Returns { context, page, frames, rtcCount }.
/**
 * True for a console line the ENGINE writes about a socket the test itself
 * expects to fail. Firefox reports "The connection to ws://... was
 * interrupted while the page was loading" when a WebSocket is closed before
 * the document finished loading — which is exactly what a ghost room does,
 * and what closing the context does to a live room. The app's own errors say
 * something else and are never filtered: this list names ENGINE text, one
 * pattern per case, so a real failure cannot hide behind it.
 */
function isEngineNoise(text) {
  return /was interrupted while the page was loading/i.test(text);
}

async function openRoom(browser, url, {rtcHook = true, init} = {}) {
  const context = await browser.newContext();
  if (rtcHook) {
    await context.addInitScript(() => {
      window.__rtcConstructed = 0;
      const RealRTC = window.RTCPeerConnection;
      if (RealRTC) {
        window.RTCPeerConnection = function (...args) {
          window.__rtcConstructed += 1;
          return new RealRTC(...args);
        };
      }
      window.__notificationRequests = 0;
      if (window.Notification && window.Notification.requestPermission) {
        const original = window.Notification.requestPermission.bind(window.Notification);
        window.Notification.requestPermission = (...args) => {
          window.__notificationRequests += 1;
          return original(...args);
        };
      }
      window.__clipboard = null;
      const stub = {
        writeText: async (text) => {
          window.__clipboard = text;
        },
      };
      try {
        Object.defineProperty(navigator, "clipboard", { value: stub, configurable: true });
      } catch {
        try {
          navigator.clipboard.writeText = stub.writeText;
        } catch {
          /* read-only native clipboard: the copy click still toasts */
        }
      }
    });
  }
  if (init) {
    await context.addInitScript(init);
  }
  const page = await context.newPage();
  const failures = [];
  const frames = { sent: [], received: [], urls: [] };
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error" && !isEngineNoise(message.text())) {
      failures.push(`console: ${message.text()}`);
    }
  });
  page.on("websocket", (socket) => {
    frames.urls.push(socket.url());
    socket.on("framesent", (frame) => frames.sent.push(frame.payload));
    socket.on("framereceived", (frame) => frames.received.push(frame.payload));
  });
  await page.goto(url);
  return { context, page, failures, frames };
}

function outboundTypes(frames) {
  const types = new Set();
  for (const payload of frames.sent) {
    try {
      types.add(JSON.parse(String(payload)).type);
    } catch {
      /* non-JSON frame: reported separately */
    }
  }
  return [...types].sort();
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 10_000 });
}

test.describe.serial("room", () => {
  async function peerCount(page) {
    return page.locator("#peer-list li").count();
  }

  test("valid link joins and preserves the short fragment without storage", async ({
    browser,
  }) => {
    const peer = await openRoom(browser, roomUrl);
    const { page, failures } = peer;
    await expectConnected(page);
    // The capability stays in the address bar and derived secrets stay only
    // in module memory, including after the session reaches the ready state.
    expect(page.url()).toBe(roomUrl);
    expect(page.url()).toContain(`#${roomSeedText}`);
    const storage = await page.evaluate(() => ({
      session: window.sessionStorage.length,
      local: window.localStorage.length,
    }));
    expect(storage).toEqual({ session: 0, local: 0 });
    // Self peer renders with the default name.
    await expect(page.locator("#peer-list li")).toHaveCount(1);
    // Production path: no development note element at all.
    await expect(page.locator('[data-testid="transfer-note"]')).toHaveCount(0);
    // Keyboard-only: the rename input takes focus and a native Enter
    // submits the form (dropzone additionally handles Enter/Space).
    await page.locator("#rename-input").focus();
    await expect(page.locator("#rename-input")).toBeFocused();
    await page.keyboard.type("Keys");
    await page.keyboard.press("Enter");
    await expect(page.locator("#peer-list")).toContainText("Keys", { timeout: 10_000 });
    expect(failures).toEqual([]);
    await peer.context.close();
  });

  test("three peers appear, rename and leave", async ({ browser }) => {
    const a = await openRoom(browser, roomUrl);
    const b = await openRoom(browser, roomUrl);
    const c = await openRoom(browser, roomUrl);
    await expect(a.page.locator("#peer-list li")).toHaveCount(3, { timeout: 10_000 });
    await expect(b.page.locator("#peer-list li")).toHaveCount(3, { timeout: 10_000 });
    // Rename from A is visible on B.
    await a.page.locator("#rename-input").fill("Alice");
    await a.page.locator("#rename-form").evaluate((form) => form.requestSubmit());
    await expect(b.page.locator("#peer-list")).toContainText("Alice", { timeout: 10_000 });
    // Publish affordances stay enabled for every peer, downloads absent.
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator("#add-file")).toBeEnabled();
      await expect(peer.page.locator("#add-folder")).toBeEnabled();
      expect(await peer.page.locator("#catalog button").count()).toBe(0);
    }
    // C leaves; A and B converge back to two peers.
    await c.context.close();
    await expect(a.page.locator("#peer-list li")).toHaveCount(2, { timeout: 10_000 });
    await expect(b.page.locator("#peer-list li")).toHaveCount(2, { timeout: 10_000 });
    expect(a.failures).toEqual([]);
    expect(b.failures).toEqual([]);
    expect(c.failures).toEqual([]);
    await a.context.close();
    await b.context.close();
  });

  test("reload in the same tab reconnects", async ({ browser }) => {
    const peer = await openRoom(browser, roomUrl);
    await expectConnected(peer.page);
    await peer.page.reload();
    await expectConnected(peer.page);
    await expect(peer.page.locator("#peer-list li")).toHaveCount(1, { timeout: 10_000 });
    expect(peer.page.url()).toBe(roomUrl);
    expect(peer.failures).toEqual([]);
    await peer.context.close();
  });

  test("copied link opens another context", async ({ browser }) => {
    const peer = await openRoom(browser, roomUrl);
    await expectConnected(peer.page);
    await peer.page.locator("#copy-link").click();
    await expect(peer.page.locator("#app-toast")).toContainText("copiato", { timeout: 10_000 });
    const copied = await peer.page.evaluate(() => window.__clipboard);
    expect(copied).toBe(roomUrl);
    expect(peer.page.url()).toBe(roomUrl);
    const other = await openRoom(browser, copied);
    await expectConnected(other.page);
    await expect(peer.page.locator("#peer-list li")).toHaveCount(2, { timeout: 10_000 });
    expect(other.failures).toEqual([]);
    await other.context.close();
    expect(peer.failures).toEqual([]);
    await peer.context.close();
  });

  test("test hook note renders only in test builds", async ({ browser }) => {
    const context = await browser.newContext();
    await context.addInitScript(() => {
      window.__BORE_TEST__ = { transferNote: "Trasferimenti disponibili nella prossima versione" };
    });
    const page = await context.newPage();
    const failures = [];
    page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
    await page.goto(roomUrl);
    await expect(page.locator('[data-testid="transfer-note"]')).toContainText(
      "Trasferimenti disponibili nella prossima versione",
      { timeout: 10_000 },
    );
    expect(failures).toEqual([]);
    await context.close();
  });

  test("remote strings render as text, never markup", async ({ browser }) => {
    const peer = await openRoom(browser, roomUrl);
    await expectConnected(peer.page);
    const evil = "<svg onload=alert(1)>";
    await peer.page.locator("#rename-input").fill(evil);
    await peer.page.locator("#rename-form").evaluate((form) => form.requestSubmit());
    await expect(peer.page.locator("#peer-list")).toContainText(evil, { timeout: 10_000 });
    expect(await peer.page.locator("#peer-list svg").count()).toBe(0);
    expect(await peer.page.locator("#catalog script").count()).toBe(0);
    expect(peer.failures).toEqual([]);
    await peer.context.close();
  });

  test("invalid links never connect", async ({ browser }) => {
    const malformed = [
      ["21 chars", `${roomUrl.slice(0, roomUrl.indexOf("#") + 1)}${roomSeedText.slice(0, 21)}`],
      ["23 chars", `${roomUrl.slice(0, roomUrl.indexOf("#") + 1)}${"A".repeat(23)}`],
      ["padding bits", `${roomUrl.slice(0, roomUrl.indexOf("#") + 1)}${roomSeedText.slice(0, -1)}x`],
      ["equals", `${roomUrl}=`],
      ["plus", `${roomUrl.slice(0, -1)}+`],
      ["slash", `${roomUrl.slice(0, -1)}/`],
      ["space escape", `${roomUrl.slice(0, -1)}%20`],
      ["unicode", `${roomUrl.slice(0, -1)}é`],
      ["empty hash", roomUrl.slice(0, roomUrl.indexOf("#") + 1)],
      ["hash query", `${roomUrl}?query=1`],
      ["path query", `${roomUrl.slice(0, roomUrl.indexOf("#")).replace("/transfer/", "/transfer/?room=1")}${roomUrl.slice(roomUrl.indexOf("#"))}`],
      ["legacy path", `${roomUrl.slice(0, roomUrl.indexOf("/transfer/"))}/transfer/c5e230000f48c492799fe9ea32d18d8c#m=${"a".repeat(64)}&k=${"b".repeat(64)}`],
      ["legacy fragment", `${roomUrl.slice(0, roomUrl.indexOf("#"))}#m=${"a".repeat(64)}&k=${"b".repeat(64)}`],
    ];
    for (const [label, href] of malformed) {
      const broken = await openRoom(browser, href);
      await expect(broken.page.locator("#room-status")).toContainText("Link incompleto", {
        timeout: 10_000,
      });
      expect(broken.frames.urls, label).toEqual([]);
      expect(broken.failures, label).toEqual([]);
      await broken.context.close();
    }

    // Unknown room, valid shape: no welcome, room becomes unavailable.
    const ghost = `http://127.0.0.1:${port}/transfer/#AAAAAAAAAAAAAAAAAAAAAA`;
    const lost = await openRoom(browser, ghost);
    await expect(lost.page.locator("#room-status")).toContainText("non disponibile", {
      timeout: 15_000,
    });
    const inbound = lost.frames.received.map((payload) => {
      try {
        return JSON.parse(String(payload)).type;
      } catch {
        return "?";
      }
    });
    expect(inbound).not.toContain("welcome");
    expect(lost.failures).toEqual([]);
    await lost.context.close();
    // An old link must not recover through browser storage. It is rejected
    // before the control socket, even when a legacy-looking value is present.
    const broken = await openRoom(
      browser,
      `http://127.0.0.1:${port}/transfer/#m=1`,
      {
        init: () => {
          sessionStorage.setItem("bore.transfer.legacy", JSON.stringify({ member: "a", key: "b" }));
        },
      },
    );
    await expect(broken.page.locator("#room-status")).toContainText("Link incompleto", {
      timeout: 10_000,
    });
    expect(broken.frames.urls).toEqual([]);
    expect(await broken.page.evaluate(() => sessionStorage.length)).toBe(1);
    expect(broken.failures).toEqual([]);
    await broken.context.close();
  });

  test("WebCrypto failure is stable, preserves the hash and never connects", async ({ browser }) => {
    const failed = await openRoom(browser, roomUrl, {
      init: () => {
        const realDeriveBits = crypto.subtle.deriveBits.bind(crypto.subtle);
        Object.defineProperty(crypto.subtle, "deriveBits", {
          configurable: true,
          value: async (...args) => {
            void realDeriveBits;
            void args;
            throw new Error("test HKDF failure");
          },
        });
      },
    });
    await expect(failed.page.locator("#room-status")).toContainText(
      "WebCrypto HKDF non disponibile",
      { timeout: 10_000 },
    );
    expect(failed.page.url()).toBe(roomUrl);
    expect(failed.frames.urls).toEqual([]);
    expect(failed.failures).toEqual([]);
    await failed.context.close();
  });

  test("a name being typed survives a room event", async ({ browser }) => {
    // `render` runs on every room event, and it used to overwrite the name
    // field unconditionally — so a peer joining while someone typed emptied
    // the box and, on submit, re-sent the OLD name. The gate is a real event
    // landing BETWEEN the typing and the submit, which is the production
    // shape and is deterministic; the flake it caused elsewhere was not.
    const typist = await openRoom(browser, roomUrl);
    await expectConnected(typist.page);
    await typist.page.locator("#rename-input").fill("Dattilografo");
    const before = await typist.page.locator("#peer-list li").count();
    const bystander = await openRoom(browser, roomUrl);
    await expectConnected(bystander.page);
    // The join reached the typist's page: its roster grew, so a render ran
    // between the typing and the submit below.
    await expect(typist.page.locator("#peer-list li")).toHaveCount(before + 1, {
      timeout: 10_000,
    });
    await expect(typist.page.locator("#rename-input")).toHaveValue("Dattilografo");
    await typist.page.locator("#rename-form").evaluate((form) => form.requestSubmit());
    await expect(typist.page.locator("#peer-list")).toContainText("Dattilografo", {
      timeout: 10_000,
    });
    expect(typist.failures).toEqual([]);
    await bystander.context.close();
    await typist.context.close();
  });

  test("no transfer, rtc, relay or notification surface", async ({ browser }) => {
    const peer = await openRoom(browser, roomUrl);
    await expectConnected(peer.page);
    await peer.page.locator("#rename-input").fill("Audit");
    await peer.page.locator("#rename-form").evaluate((form) => form.requestSubmit());
    await expect(peer.page.locator("#peer-list")).toContainText("Audit", { timeout: 10_000 });
    const { page, frames, failures } = peer;
    // Every socket is the control socket; every outbound frame is
    // hello/ping/rename (pings may not have fired yet — subset, not equal).
    expect(frames.urls.length).toBeGreaterThanOrEqual(1);
    for (const url of frames.urls) {
      expect(url).toContain("/transfer/ws/control/");
    }
    for (const type of outboundTypes(frames)) {
      expect(["hello", "peer.rename", "ping"]).toContain(type);
    }
    // No peer connection was ever constructed.
    expect(await page.evaluate(() => window.__rtcConstructed)).toBe(0);
    // No OS notification permission was ever requested (the ambient state
    // is environment-controlled; what matters is zero requests from us).
    expect(await page.evaluate(() => window.__notificationRequests)).toBe(0);
    // Console stayed clean and secrets never leaked into it.
    for (const failure of failures) {
      expect(failure).not.toContain(memberToken);
    }
    expect(failures).toEqual([]);
    await peer.context.close();
  });
});
