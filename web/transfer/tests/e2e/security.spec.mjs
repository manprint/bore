// T-WEB-E2EE / T-WEB-ROOM-LIFE, browser half (3.7): the adversarial cases a
// Rust test cannot reach, because the attacker here is the PAGE — what it
// puts on the wire, what it does with a capability it should not have, and
// what it does when the room goes away underneath it.
//
// Four claims, one test each:
//   1. the room key never leaves the tab and the member token leaves once;
//   2. a peer holding the wrong room key learns nothing from an offer;
//   3. a spent relay ticket is refused, and so is the same ticket in the
//      other role;
//   4. losing the room mid-download fails the download instead of handing
//      the user a half-file.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnRoomEnv, openPersistentPeer, opfsWorks } from "./helpers.mjs";

let env = null;
let roomDir = null;

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-security-"));
  const bytes = Buffer.alloc(512 * 1024);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = (i * 11 + 5) % 251;
  }
  writeFileSync(join(roomDir, "secret.bin"), bytes);
}, 120_000);

test.afterAll(() => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

// Records every outbound WebSocket payload before the app boots. Test-only:
// production ships no such wrapper, and the app never reads it back.
const RECORD_SENT = () => {
  window.__SEC__ = { sent: [] };
  const realSend = WebSocket.prototype.send;
  WebSocket.prototype.send = function (data) {
    try {
      window.__SEC__.sent.push(typeof data === "string" ? data : "<binary>");
    } catch {
      /* recording must never break the socket */
    }
    return realSend.call(this, data);
  };
};

// Test-only fault injection: preserve the seed-derived RoomId/member token,
// but flip one bit of the independent RoomKey HKDF output. The production
// bundle never installs this wrapper; the browser context is torn down after
// the assertion, so the mutation cannot escape the test.
const FLIP_ROOM_KEY = () => {
  const subtle = globalThis.crypto?.subtle;
  if (!subtle) {
    return;
  }
  const realDeriveBits = subtle.deriveBits.bind(subtle);
  Object.defineProperty(subtle, "deriveBits", {
    configurable: true,
    value: async (params, key, length) => {
      const bits = await realDeriveBits(params, key, length);
      const info = new TextDecoder().decode(params?.info ?? new Uint8Array());
      if (info !== "bore-web-transfer-room-key-v1") {
        return bits;
      }
      const bytes = new Uint8Array(bits);
      bytes[0] ^= 1;
      return bytes.buffer;
    },
  });
};

async function openPeer(url, init) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  // The security gates are written against the RELAY leg (opaque frames,
  // attach authorization, room death mid-transfer), so these peers run
  // without WebRTC and the transfer falls back at once.
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    init,
    noWebRtc: true,
  });
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 30_000 });
}

async function poll(page, fn, arg, timeoutMs = 30_000) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn, arg);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("security poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

test.describe.serial("web transfer security", () => {
  test("the room key never leaves the tab and the token leaves once", async () => {
    const a = await openPeer(env.roomUrl, RECORD_SENT);
    const b = await openPeer(env.roomUrl, RECORD_SENT);
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "secret.bin")]);
    const offer = await poll(b.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    expect(await b.page.evaluate((id) => window.__BORE_TEST__.requestDownload(id), offer.offerId))
      .toEqual({ pending: true });
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });

    for (const [name, peer] of [
      ["A", a],
      ["B", b],
    ]) {
      const sent = await peer.page.evaluate(() => window.__SEC__.sent);
      const urls = await peer.page.evaluate(() => [...(window.__BORE_TEST__.wsUrls ?? [])]);
      // The key is the whole of the confidentiality claim: it decrypts every
      // payload byte and every manifest, and it exists only in this tab.
      for (const frame of sent) {
        expect(frame, `${name} put the room key on the wire`).not.toContain(env.roomKey);
      }
      for (const url of urls) {
        expect(url, `${name} put a secret in a URL`).not.toContain(env.roomKey);
        expect(url, `${name} put the member token in a URL`).not.toContain(env.memberToken);
      }
      // The token is a bearer credential for the room: it authenticates the
      // hello and must never be repeated afterwards.
      const carrying = sent.filter((frame) => frame.includes(env.memberToken));
      expect(carrying.length, `${name} sent the member token ${carrying.length} times`).toBe(1);
      expect(sent.indexOf(carrying[0]), `${name} sent the token after the hello`).toBe(0);
      expect(JSON.parse(carrying[0]).type).toBe("hello");
      // And the URL bar no longer holds either of them.
      const href = await peer.page.evaluate(() => window.location.href);
      expect(href).not.toContain(env.roomKey);
      expect(href).not.toContain(env.memberToken);
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

test("a peer with the wrong room key learns nothing from an offer", async () => {
    const a = await openPeer(env.roomUrl);
    // Same room, same member token, one flipped nibble of the room key: the
    // server cannot tell the difference, which is the point.
    const bad = await openPeer(env.roomUrl, FLIP_ROOM_KEY);
    await expectConnected(a.page);
    await expectConnected(bad.page);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "secret.bin")]);
    // The good peer publishes; give the bad peer the same broadcast plus a
    // generous settle window, then assert it holds nothing.
    const good = await openPeer(env.roomUrl);
    await expectConnected(good.page);
    await poll(good.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    await new Promise((r) => setTimeout(r, 1500));
    expect(await bad.page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot())).toEqual([]);
    // The other entry point: a peer that joins AFTER the publish receives the
    // same offer in its join snapshot, and must refuse it there too.
    const late = await openPeer(env.roomUrl, FLIP_ROOM_KEY);
    await expectConnected(late.page);
    await new Promise((r) => setTimeout(r, 1500));
    expect(await late.page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot())).toEqual([]);
    // It is not silently broken either: it refused a manifest it could not
    // authenticate, and says so.
    const errors = await bad.page.evaluate(() => [...(window.__BORE_TEST__.controlErrors ?? [])]);
    expect(Array.isArray(errors)).toBe(true);
    // A peer that cannot read the catalog cannot start a download: there is
    // no offer to name.
    expect(
      await bad.page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot().length),
    ).toBe(0);

    for (const peer of [a, good, bad, late]) {
      await peer.cleanup();
    }
  });

  test("a spent ticket is refused, and so is the same ticket in the other role", async () => {
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "secret.bin")]);
    const offer = await poll(b.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    await b.page.evaluate((id) => window.__BORE_TEST__.requestDownload(id), offer.offerId);
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });

    // The recipient's own ticket, now spent by the completed transfer.
    const ticket = await poll(b.page, () => window.__BORE_TEST__.relayTickets.at(-1) ?? null);
    const peerId = await b.page.evaluate(() => window.__BORE_TEST__.selfPeerId);
    expect(ticket.ticket).toMatch(/^[0-9a-f]{32}$/);

    // Both attacks are one function with one variable: the role. A refusal
    // is a close with no frame — the server never explains itself here.
    const attack = async (role) =>
      b.page.evaluate(
        ([roomId, attach]) =>
          new Promise((resolve) => {
            const socket = new WebSocket(
              `${location.origin.replace(/^http/, "ws")}/transfer/ws/relay/${roomId}/${attach.transferId}`,
              "bore-transfer-v1",
            );
            socket.binaryType = "arraybuffer";
            let frames = 0;
            const done = (verdict) => resolve({ verdict, frames });
            const timer = setTimeout(() => {
              socket.close();
              done("open");
            }, 8000);
            socket.onopen = () => socket.send(JSON.stringify(attach));
            socket.onmessage = () => {
              frames += 1;
            };
            socket.onclose = () => {
              clearTimeout(timer);
              done("closed");
            };
            socket.onerror = () => {
              clearTimeout(timer);
              done("closed");
            };
          }),
        [
          env.roomId,
          {
            v: 1,
            peerId,
            transferId: ticket.transferId,
            attemptId: ticket.attemptId,
            role,
            ticket: ticket.ticket,
          },
        ],
      );

    const replay = await attack("recipient");
    expect(replay.verdict, "a spent ticket was accepted again").toBe("closed");
    expect(replay.frames).toBe(0);
    const swapped = await attack("source");
    expect(swapped.verdict, "a recipient ticket was accepted as a source").toBe("closed");
    expect(swapped.frames).toBe(0);

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });

  test("losing the room mid-download fails the download", async () => {
    // Owner grace (5 s) plus the reconnect backoff that follows it: this one
    // waits on real time and cannot fit the default per-test budget.
    test.setTimeout(150_000);
    // Its own server: the room must die WHILE bytes move, which needs a
    // short owner grace and a payload slow enough to still be in flight.
    // Grace 5 s is the server's own floor; the payload is sized so it is
    // still moving well past it (16 MiB through a 1 MiB/s relay).
    const live = await spawnRoomEnv({ ownerGrace: 5, relayRate: 1024 * 1024 });
    const dir = mkdtempSync(join(tmpdir(), "bore-security-live-"));
    try {
      const bytes = Buffer.alloc(16 * 1024 * 1024);
      for (let i = 0; i < bytes.length; i += 1) {
        bytes[i] = (i * 3 + 1) % 251;
      }
      writeFileSync(join(dir, "slow.bin"), bytes);
      const a = await openPeer(live.roomUrl);
      const b = await openPeer(live.roomUrl);
      await expectConnected(a.page);
      await expectConnected(b.page);
      expect(await opfsWorks(b.page)).toBe(true);

      await a.page.locator("#file-input").setInputFiles([join(dir, "slow.bin")]);
      const offer = await poll(b.page, () => {
        const catalog = window.__BORE_TEST__.getCatalogSnapshot();
        return catalog.length === 1 ? catalog[0] : null;
      });
      await b.page.evaluate((id) => window.__BORE_TEST__.requestDownload(id), offer.offerId);
      // Wait for real progress, so the room dies on a RUNNING relay and not
      // on a transfer that had not started.
      await poll(b.page, () => {
        const rows = window.__BORE_TEST__.receiverState();
        return rows.some((row) => row.receivedBytes > 0);
      });

      live.killOwner();
      // Grace 5 s: the room is gone well inside this window, and with it the
      // transfer. The page must not end up offering a partial file.
      // Measured 5.3 s from the kill on this machine: the socket drops when
      // the room expires, and the reconnect is then refused for good — the
      // page must land on the terminal state, not spin on "Riconnessione".
      await expect(b.page.locator("#room-status")).toContainText("non disponibile", {
        timeout: 60_000,
      });
      await expect(b.page.locator("#save-file")).toHaveCount(0);
      const rows = await b.page.evaluate(() => window.__BORE_TEST__.receiverState());
      expect(rows.every((row) => row.state !== "VERIFIED")).toBe(true);

      for (const peer of [a, b]) {
        await peer.cleanup();
      }
    } finally {
      live.cleanup();
      rmSync(dir, { recursive: true, force: true });
    }
  });

  // T-WEB-XSS-CSRF -------------------------------------------------------
  //
  // Two boundaries in one test, because they fail the same way: something
  // from outside the room getting the page to act on its behalf.
  //
  // CSRF first. A WebSocket upgrade is NOT subject to the same-origin policy
  // the way `fetch` is: any page anywhere can open one at this server. What a
  // foreign page cannot do is forge its `Origin`, so the server's exact-origin
  // check is the whole boundary — and it is tested from a REAL foreign page,
  // because a hand-written header would be testing the test.
  //
  // Then XSS, on the two strings that travel between peers and are chosen by
  // a human: the display name and the offer label. Both must arrive as TEXT,
  // and the bidi override characters that can make `report.txt` read as
  // `report.exe` must not survive into the label either.
  test("T-WEB-XSS-CSRF a foreign origin cannot open the control socket, and remote text never becomes markup or a spoofed name", async ({
    browser,
  }) => {
    // --- CSRF ----------------------------------------------------------
    // The attacker page must come from a real foreign ORIGIN, so it is
    // served by a throwaway HTTP server of its own: bore answers nothing on
    // a `Host` it does not advertise (the room shell is same-origin by
    // construction), so a second port of the SAME server cannot stage this.
    const evil = createServer((_req, res) => {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end("<!doctype html><title>evil</title>");
    });
    await new Promise((resolve) => evil.listen(0, "127.0.0.1", resolve));
    const evilPort = evil.address().port;
    const foreign = await browser.newContext();
    const attacker = await foreign.newPage();
    await attacker.goto(`http://127.0.0.1:${evilPort}/`);
    const outcome = await attacker.evaluate(
      ({ port, roomId }) =>
        new Promise((resolve) => {
          let socket;
          try {
            socket = new WebSocket(
              `ws://127.0.0.1:${port}/transfer/ws/control/${roomId}`,
              "bore-transfer-v1",
            );
          } catch (error) {
            resolve({ threw: String(error) });
            return;
          }
          const done = (what) => resolve(what);
          socket.onopen = () => done({ opened: true });
          socket.onerror = () => done({ refused: true });
          socket.onclose = (event) => done({ closed: event.code });
          setTimeout(() => done({ silent: true }), 10_000);
        }),
      { port: env.port, roomId: env.roomId },
    );
    expect(outcome.opened, `a foreign origin opened the control socket: ${JSON.stringify(outcome)}`)
      .toBeUndefined();
    await foreign.close();
    await new Promise((resolve) => evil.close(resolve));

    // --- XSS and name spoofing ------------------------------------------
    const markup = '<img src=x onerror="window.__PWNED__=1">';
    // RLO + PDF around the extension: the classic filename spoof.
    const bidi = "invoice\u202Efdp.exe\u202C";
    const dir = mkdtempSync(join(tmpdir(), "bore-xss-"));
    try {
      writeFileSync(join(dir, "plain.bin"), Buffer.alloc(2048, 7));
      const a = await openPeer(env.roomUrl);
      const b = await openPeer(env.roomUrl);

      // A name carrying markup, published by the OTHER peer.
      await a.page.locator("#rename-input").fill(`${markup}${bidi}`);
      await a.page.locator("#rename-form").evaluate((form) => form.requestSubmit());
      await expect(b.page.locator("#peer-list")).toContainText("invoice", { timeout: 15_000 });
      expect(await b.page.locator("#peer-list img").count()).toBe(0);
      expect(await b.page.evaluate(() => window.__PWNED__)).toBeUndefined();

      // And an offer whose label is the same hostile string.
      await a.page.setInputFiles("#file-input", join(dir, "plain.bin"));
      await expect(b.page.locator("#catalog")).toContainText("plain.bin", { timeout: 20_000 });
      expect(await b.page.locator("#catalog img").count()).toBe(0);
      expect(await b.page.locator("#catalog script").count()).toBe(0);
      expect(await b.page.evaluate(() => window.__PWNED__)).toBeUndefined();

      // The bidi overrides are not carried into what the reader sees: a
      // label that can reorder itself is a label that can lie about what it
      // is, and this page is where the reader decides to download.
      const shown = await b.page.locator("#peer-list").innerText();
      expect(shown.includes("\u202E"), "an RLO override survived into the peer list").toBe(false);
      expect(shown.includes("\u202C"), "a PDF override survived into the peer list").toBe(false);

      // Neither page logged an error, so nothing above was "safe" because
      // the app broke.
      expect(a.failures).toEqual([]);
      expect(b.failures).toEqual([]);
      for (const peer of [a, b]) {
        await peer.cleanup();
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
