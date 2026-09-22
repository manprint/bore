// T-WEB-SENDER-RELAY: after B's explicit click the source auto-accepts and
// streams one multi-chunk file over the relay; nothing is read before the
// click or before path_commit, and a changed source fails without payload.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash, randomBytes } from "node:crypto";
import { spawnRoomEnv, canonicalize } from "./helpers.mjs";
import { disableWebRtc, installTestHooks, hookCounters } from "./fixtures.js";

let env = null;
let roomDir = null;
let fileBytes = null;
let fileHashHex = null;

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-sender-"));
  // Three chunks (2 MiB + 7), deterministic content.
  fileBytes = Buffer.alloc(2 * 1024 * 1024 + 7);
  for (let i = 0; i < fileBytes.length; i++) {
    fileBytes[i] = i % 251;
  }
  fileHashHex = createHash("sha256").update(fileBytes).digest("hex");
  writeFileSync(join(roomDir, "big.bin"), fileBytes);
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function openPeer(browser, url) {
  const context = await browser.newContext();
  await installTestHooks(context);
  // The relay gate: direct is the default since 4.2, so these peers present
  // as an engine without WebRTC and fall back immediately.
  await disableWebRtc(context);
  const page = await context.newPage();
  const failures = [];
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error") {
      failures.push(`console: ${message.text()}`);
    }
  });
  await page.goto(url);
  return { context, page, failures };
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 15_000 });
}

async function poll(page, fn, arg, timeoutMs = 20_000) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn, arg);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("e2e poll timed out");
    }
    await new Promise((r) => setTimeout(r, 100));
  }
}

function selectionDigestHex(offerId, macHex) {
  const canonical = canonicalize({ entryIds: ["0"], manifestMac: macHex, mode: "raw", offerId });
  return createHash("sha256").update(canonical, "utf8").digest("hex");
}

test.describe.serial("sender-relay", () => {
  test("click auto-accepts, streams exact bytes, change fails clean", async ({ browser }) => {
    const a = await openPeer(browser, env.roomUrl);
    const b = await openPeer(browser, env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);

    // A publishes the multi-chunk file.
    await a.page.locator("#file-input").setInputFiles([join(roomDir, "big.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const offerId = offer.offerId;
    const readsAfterPublish = (await hookCounters(a.page)).fileReads;

    // B clicks: explicit request through the test-only control sender.
    const requestId = randomBytes(16).toString("hex");
    const digest = selectionDigestHex(offerId, offer.mac);
    await b.page.evaluate(
      ([rid, oid, dgst]) =>
        window.__BORE_TEST__.controlSend("transfer.request", rid, {
          offerId: oid,
          entryIds: ["0"],
          selectionDigest: dgst,
          mode: "raw",
        }),
      [requestId, offerId, digest],
    );

    // A auto-accepts with no prompt: ready leaves, and no payload byte is
    // read before the click… (already true) …or while pairing is still
    // impossible (B has not attached yet, so no commit can exist).
    await poll(a.page, () =>
      window.__BORE_TEST__.outboundTypes.includes("transfer.source_ready"),
    );
    expect((await hookCounters(a.page)).fileReads).toBe(readsAfterPublish);

    // B attaches by hand at ticket time (pairing needs both legs) and starts
    // collecting at once; decryption runs in-page on an independent
    // WebCrypto path — no app code involved.
    const ticket = await poll(b.page, () => {
      const found = (window.__BORE_TEST__.relayTickets ?? []).find(
        (t) => t.transferId !== undefined,
      );
      return found ?? null;
    });
    // Still no commit possible (B leg not open): still no reads.
    expect((await hookCounters(a.page)).fileReads).toBe(readsAfterPublish);
    await b.page.evaluate(
      ({ roomId, peerId, transferId, attemptId, ticketHex, roomKeyHex, expectedHash }) => {
        const sub = (name, bytes) =>
          crypto.subtle.digest(name, bytes).then((d) => new Uint8Array(d));
        const hkdf = async (ikm, salt, info) => {
          const key = await crypto.subtle.importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
          const bits = await crypto.subtle.deriveBits(
            { name: "HKDF", hash: "SHA-256", salt, info },
            key,
            256,
          );
          return new Uint8Array(bits);
        };
        const hex = (bytes) => [...bytes].map((x) => x.toString(16).padStart(2, "0")).join("");
        const unhex = (s) => Uint8Array.from(s.match(/../g).map((b) => parseInt(b, 16)));
        window.__relayDone = (async () => {
          const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
          const url = `${scheme}//${window.location.host}/transfer/ws/relay/${roomId}/${transferId}`;
          const info = new Uint8Array(32);
          info.set(unhex(transferId), 0);
          info.set(unhex(attemptId), 16);
          const attemptKey = await hkdf(
            unhex(roomKeyHex),
            new TextEncoder().encode("bore-web-attempt-v1"),
            info,
          );
          const aes = await crypto.subtle.importKey("raw", attemptKey, "AES-GCM", false, [
            "decrypt",
          ]);
          return new Promise((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error("relay receive timed out")), 60_000);
            let settled = false;
            const settle = (fn, value) => {
              if (!settled) {
                settled = true;
                clearTimeout(timer);
                fn(value);
              }
            };
            const socket = new WebSocket(url, "bore-transfer-v1");
            const chunks = [];
            let expectedSeq = 0;
            let total = null;
            let opened = false;
            let errorSeen = null;
            // Message events dispatch in order but their async continuations
            // would interleave: chain them so sequencing and the final drain
            // are deterministic.
            let chain = Promise.resolve();
            const handleMessage = async (event) => {
              const msg = new Uint8Array(await event.data.arrayBuffer());
              const view = new DataView(msg.buffer);
              if (view.getUint32(0, false) !== 0x42575431 || view.getUint16(4, false) !== 1) {
                throw new Error("bad frame header");
              }
              const ftype = msg[6];
              const seq = view.getUint32(8, false);
              const bodyLen = view.getUint32(12, false);
              if (seq !== expectedSeq || bodyLen !== msg.length - 16) {
                throw new Error(`bad sequence or length at ${seq}`);
              }
              expectedSeq += 1;
              const header = msg.slice(0, 16);
              const body = msg.slice(16);
              const nonce = new Uint8Array(12);
              new DataView(nonce.buffer).setBigUint64(0, BigInt(seq), false);
              const plaintext = new Uint8Array(
                await crypto.subtle.decrypt(
                  { name: "AES-GCM", iv: nonce, additionalData: header },
                  aes,
                  body,
                ),
              );
              if (ftype === 1) {
                chunks.push(plaintext);
              } else if (ftype === 2) {
                total = new DataView(plaintext.buffer).getBigUint64(0, false);
              } else {
                throw new Error(`bad frame type ${ftype}`);
              }
            };
            socket.onopen = () => {
              opened = true;
              socket.send(
                JSON.stringify({
                  v: 1,
                  peerId,
                  transferId,
                  attemptId,
                  role: "recipient",
                  ticket: ticketHex,
                }),
              );
            };
            socket.onmessage = (event) => {
              chain = chain.then(() => handleMessage(event), (error) => {
                settle(reject, error);
                throw error;
              });
            };
            socket.onclose = () => {
              chain.then(
                async () => {
                  try {
                    let length = 0;
                    for (const c of chunks) {
                      length += c.length;
                    }
                    const flat = new Uint8Array(length);
                    let at = 0;
                    for (const c of chunks) {
                      flat.set(c, at);
                      at += c.length;
                    }
                    const hash = hex(new Uint8Array(await sub("SHA-256", flat)));
                    settle(resolve, {
                      frames: expectedSeq,
                      bytes: flat.length,
                      total: total === null ? null : Number(total),
                      dataOk: hash === expectedHash,
                      errorSeen,
                    });
                  } catch (error) {
                    settle(reject, error);
                  }
                },
                (error) => settle(reject, error),
              );
            };
            socket.onerror = (event) => {
              // WebKit reports the closing handshake as an error even when
              // every byte arrived: only a pre-open error fails fast. Past
              // that the close carries the verdict and the asserts decide.
              if (!opened) {
                settle(reject, new Error("relay socket error"));
              } else {
                errorSeen = String(event?.type ?? "error");
              }
            };
          });
        })();
      },
      {
        roomId: env.roomId,
        peerId: await b.page.evaluate(() => window.__BORE_TEST__.selfPeerId),
        transferId: ticket.transferId,
        attemptId: ticket.attemptId,
        ticketHex: ticket.ticket,
        roomKeyHex: env.roomKey,
        expectedHash: fileHashHex,
      },
    );
    // Pairing completes, then the pipeline starts reading.
    await poll(a.page, () => window.__BORE_TEST__.inboundTypes.includes("transfer.path_commit"));
    // The sender opened its relay leg on the ticket (pairing needs it).
    await poll(a.page, () =>
      window.__BORE_TEST__.wsUrls.some((url) => url.includes("/transfer/ws/relay/")),
    );
    const received = await b.page.evaluate(() => window.__relayDone);
    // Three chunks → 43 + 43 + 1 DATA fragments plus FINAL.
    expect(received.frames).toBe(43 + 43 + 1 + 1);
    expect(received.bytes).toBe(fileBytes.length);
    expect(received.total).toBe(fileBytes.length);
    expect(received.dataOk).toBe(true);
    // Payload reads happened only after the commit.
    expect((await hookCounters(a.page)).fileReads).toBeGreaterThan(readsAfterPublish);

    // Retire transfer 1 (no recipient completion exists yet in 3.3): without
    // the cancel the next request would idempotently re-ack it live.
    await b.page.evaluate(
      ([tid]) =>
        window.__BORE_TEST__.controlSend("transfer.cancel", "cc".repeat(16), {
          transferId: tid,
        }),
      [ticket.transferId],
    );
    await poll(a.page, () =>
      window.__BORE_TEST__.inboundTypes.includes("transfer.cancelled"),
    );

    // Altered source: same size and mtime, flipped content. The incoming
    // validation passes, so the sender readies — the pipeline rehash is
    // what catches it, withdrawing the offer and failing the attempt.
    const entryMtime = Number(offer.manifest.entries[0].mtime) * 1000;
    const evil = Buffer.from(fileBytes);
    evil[1000] ^= 0xff;
    await a.page.evaluate(
      ([oid, bytes64, mtime]) =>
        window.__BORE_TEST__.replaceOfferFile(
          oid,
          Uint8Array.from(atob(bytes64), (c) => c.charCodeAt(0)),
          "big.bin",
          mtime,
        ),
      [offerId, evil.toString("base64"), entryMtime],
    );
    const requestId2 = randomBytes(16).toString("hex");
    await b.page.evaluate(
      ([rid, oid, dgst]) =>
        window.__BORE_TEST__.controlSend("transfer.request", rid, {
          offerId: oid,
          entryIds: ["0"],
          selectionDigest: dgst,
          mode: "raw",
        }),
      [requestId2, offerId, digest],
    );
    // B attaches for the new attempt too: without pairing there is no
    // path_commit, no pipeline, and nothing to fail.
    const ticket2 = await poll(
      b.page,
      (oldTicket) => {
        const found = (window.__BORE_TEST__.relayTickets ?? []).filter(
          (t) => t.transferId !== undefined && t.ticket !== oldTicket,
        );
        return found.length > 0 ? found[found.length - 1] : null;
      },
      ticket.ticket,
    );
    await b.page.evaluate(
      ({ roomId, peerId, transferId, attemptId, ticketHex }) => {
        const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
        const socket = new WebSocket(
          `${scheme}//${window.location.host}/transfer/ws/relay/${roomId}/${transferId}`,
          "bore-transfer-v1",
        );
        socket.onopen = () => {
          socket.send(
            JSON.stringify({
              v: 1,
              peerId,
              transferId,
              attemptId,
              role: "recipient",
              ticket: ticketHex,
            }),
          );
        };
        window.__relaySecond = new Promise((resolve) => {
          socket.onclose = () => resolve(true);
        });
        setTimeout(() => socket.close(), 30_000);
      },
      {
        roomId: env.roomId,
        peerId: await b.page.evaluate(() => window.__BORE_TEST__.selfPeerId),
        transferId: ticket2.transferId,
        attemptId: ticket2.attemptId,
        ticketHex: ticket2.ticket,
      },
    );
    // Either terminal notice is correct: the withdraw cancel and the pump
    // abort race across channels, and exactly one terminal wins (the other
    // is an idempotent no-op). B cancelled transfer 1 itself (ack only), so
    // exactly one more notice may arrive for transfer 2.
    const dumpBoth = async (tag) => {
      const dump = await a.page.evaluate(() => ({
        outbound: window.__BORE_TEST__.outboundTypes,
        inbound: window.__BORE_TEST__.inboundTypes,
        catalog: window.__BORE_TEST__.getCatalogSnapshot().map((o) => o.offerId),
        senders: window.__BORE_TEST__.senderState(),
      }));
      console.log(`A DUMP ${tag} ` + JSON.stringify(dump).slice(0, 1200));
      const dumpB = await b.page.evaluate(() => ({
        inbound: window.__BORE_TEST__.inboundTypes,
        errors: window.__BORE_TEST__.controlErrors,
      }));
      console.log(`B DUMP ${tag} ` + JSON.stringify(dumpB).slice(0, 600));
    };
    // The sender detected the change and withdrew the offer first.
    await poll(a.page, () =>
      window.__BORE_TEST__.outboundTypes.includes("offer.withdraw"),
    );
    try {
      await poll(
        b.page,
        () => {
          const cancelled = window.__BORE_TEST__.inboundTypes.filter(
            (t) => t === "transfer.cancelled",
          ).length;
          const failed = window.__BORE_TEST__.controlErrors.some(
            (e) => e.code === "DIRECT_FAILED",
          );
          return cancelled === 1 || failed;
        },
        null,
        10000,
      );
    } catch {
      await dumpBoth("notice");
      throw new Error("no terminal notice");
    }
    // The changed source is withdrawn everywhere; no payload ever flowed.
    try {
      await poll(a.page, () => window.__BORE_TEST__.getCatalogSnapshot().length === 0, null, 10000);
    } catch {
      const dump = await a.page.evaluate(() => ({
        outbound: window.__BORE_TEST__.outboundTypes,
        inbound: window.__BORE_TEST__.inboundTypes,
        catalog: window.__BORE_TEST__.getCatalogSnapshot().map((o) => o.offerId),
        senders: window.__BORE_TEST__.senderState(),
      }));
      console.log("A DUMP " + JSON.stringify(dump).slice(0, 1200));
      const dumpB = await b.page.evaluate(() => ({
        inbound: window.__BORE_TEST__.inboundTypes,
        errors: window.__BORE_TEST__.controlErrors,
      }));
      console.log("B DUMP " + JSON.stringify(dumpB).slice(0, 600));
      throw new Error("catalog did not converge");
    }

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.context.close();
    }
  });
});
