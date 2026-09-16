// T-WEB-DND / T-WEB-PATH-UI / T-WEB-UI (5.6): the interface's own gates.
//
// Three claims, each of which can only be checked in a real engine: a drop
// publishes and starts nothing, the path badge never names a transport that
// has not carried a verified byte, and the layout does not move under the
// user while a transfer runs.
import { test, expect } from "@playwright/test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openPersistentPeer, opfsWorks, spawnRoomEnv } from "./helpers.mjs";
import { hookCounters } from "./fixtures.js";

const CHUNK_BYTES = 1024 * 1024;
/** Paces the relay leg so the badge's transitions are observable. */
const SLOW_RATE = 1024 * 1024;

let env = null;
let roomDir = null;

function filler(length, salt) {
  const out = Buffer.alloc(length);
  for (let i = 0; i < length; i += 1) {
    out[i] = (i * 17 + salt * 5 + 3) % 251;
  }
  return out;
}

test.beforeAll(async () => {
  env = await spawnRoomEnv({ relayRate: SLOW_RATE });
  roomDir = mkdtempSync(join(tmpdir(), "bore-ui-"));
  // Eight chunks and a tail: a quarter of it is two WHOLE chunks, so a
  // channel killed at 25 % leaves verified work behind and the relay leg
  // that follows is long enough to watch.
  writeFileSync(join(roomDir, "grande.bin"), filler(8 * CHUNK_BYTES + 7, 1));
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function openPeer(options = {}) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(env.roomUrl, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    ...options,
  });
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", {
    timeout: 20_000,
  });
}

async function poll(page, fn, timeoutMs = 60_000) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("e2e poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

/**
 * The zones' positions in DOCUMENT coordinates. Viewport coordinates would
 * make a scroll — which the harness itself causes when it scrolls a control
 * into view before clicking it — look exactly like the layout shifting.
 * The scroll is checked separately, and only over the window in which the
 * claim applies: while a transfer runs.
 */
async function zoneBoxes(page) {
  return page.evaluate(() =>
    ["zone-room", "zone-offers", "zone-transfers", "add-file", "add-folder", "dropzone", "copy-link"].map(
      (id) => {
        const box = document.getElementById(id).getBoundingClientRect();
        return [id, Math.round(box.top + window.scrollY), Math.round(box.left + window.scrollX)];
      },
    ),
  );
}

async function publish(peer, name) {
  await peer.page.locator("#file-input").setInputFiles(join(roomDir, name));
  await expect(peer.page.locator(`.offer-card h4:has-text("${name}")`)).toBeVisible({
    timeout: 30_000,
  });
  return poll(peer.page, () => {
    const catalog = window.__BORE_TEST__.getCatalogSnapshot();
    const self = window.__BORE_TEST__.selfPeerId;
    const mine = catalog.filter((offer) => offer.peerId === self);
    return mine.length > 0 ? mine[mine.length - 1].offerId : null;
  });
}

/**
 * Records EVERY value the badge's `data-path` has held, in order, using the
 * mutation record's own `oldValue`. Reading the current value on each
 * callback would miss a transition that lands in the same batch as the next
 * one, which is exactly the `direct → connecting → relay` case this exists
 * to observe.
 */
async function watchPath(page) {
  await page.evaluate(() => {
    window.__pathTrail = [];
    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (record.type === "attributes" && record.attributeName === "data-path") {
          const previous = record.oldValue;
          if (previous !== null && window.__pathTrail.at(-1) !== previous) {
            window.__pathTrail.push(previous);
          }
        }
      }
    });
    observer.observe(document.getElementById("transfers"), {
      subtree: true,
      childList: true,
      attributes: true,
      attributeOldValue: true,
      attributeFilter: ["data-path"],
    });
  });
}

async function pathTrail(page) {
  return page.evaluate(() => {
    const badge = document.querySelector(".transfer-row .transfer-path");
    const trail = [...(window.__pathTrail ?? [])];
    const now = badge === null ? null : badge.getAttribute("data-path");
    if (now !== null && trail.at(-1) !== now) {
      trail.push(now);
    }
    return trail;
  });
}

/**
 * Init script for the RECIPIENT: closes the one DataChannel once `limit`
 * payload bytes have arrived AND a chunk has verified. The recipient creates
 * the channel, so wrapping `createDataChannel` reaches the real object the
 * app uses — nothing is faked except the moment the transport dies. Same
 * shape as `direct.spec.mjs`'s own killer; kept local because this gate
 * watches a different thing (the badge, not the bytes) and the two must be
 * able to move independently.
 */
function killChannelAfter(limit) {
  return `(() => {
    const LIMIT = ${limit};
    const Real = window.RTCPeerConnection;
    if (typeof Real !== "function") { return; }
    const create = Real.prototype.createDataChannel;
    Real.prototype.createDataChannel = function (...args) {
      const channel = create.apply(this, args);
      let seen = 0;
      // The precondition is the BADGE, not a verified chunk. A chunk is
      // verified a moment before the path is committed (the commit is what
      // makes the transport real, and for an archive it waits on a record
      // write), so killing on "a chunk verified" can land inside that window:
      // the badge then correctly never names the direct leg, the fallback has
      // nothing to walk back, and the transition this gate exists for never
      // happens. Waiting for the badge makes the scenario deterministic
      // instead of depending on how loaded the machine is. No backtick below
      // or in here: this whole function is a template literal.
      const named = () => {
        try {
          return document.querySelector(".transfer-path")?.dataset.path === "direct";
        } catch {
          return false;
        }
      };
      channel.addEventListener("message", (event) => {
        const data = event.data;
        seen += data && data.byteLength !== undefined ? data.byteLength : (data && data.size) || 0;
        if (seen >= LIMIT && channel.readyState === "open" && named()) {
          try { window.__BORE_TEST__.killedAt = seen; } catch {}
          channel.close();
        }
      });
      return channel;
    };
  })();`;
}

test.describe.serial("ui", () => {
  test("T-WEB-DND a real drop publishes one offer and starts nothing", async () => {
    test.setTimeout(180_000);
    const a = await openPeer();
    const b = await openPeer();
    for (const peer of [a, b]) {
      await expectConnected(peer.page);
    }

    // The zone answers a drag before anything is dropped on it.
    await a.page.dispatchEvent("#dropzone", "dragenter");
    await expect(a.page.locator("#dropzone")).toHaveAttribute("data-dragging", "true");
    await a.page.dispatchEvent("#dropzone", "dragleave");
    await expect(a.page.locator("#dropzone")).toHaveAttribute("data-dragging", "false");

    // ONE file, dropped as a real `drop` event carrying a real `DataTransfer`.
    const single = await a.page.evaluateHandle(() => {
      const data = new DataTransfer();
      data.items.add(new File([new Uint8Array([1, 2, 3, 4])], "uno.txt", { type: "text/plain" }));
      return data;
    });
    await a.page.dispatchEvent("#dropzone", "drop", { dataTransfer: single });
    await expect(a.page.locator('.offer-card h4:has-text("uno.txt")')).toBeVisible({
      timeout: 30_000,
    });
    await expect(a.page.locator("#dropzone")).toHaveAttribute("data-dragging", "false");

    // SEVERAL files in one drop: one offer, not one per file.
    const many = await a.page.evaluateHandle(() => {
      const data = new DataTransfer();
      data.items.add(new File([new Uint8Array([5, 6])], "due.txt", { type: "text/plain" }));
      data.items.add(new File([new Uint8Array([7, 8, 9])], "tre.txt", { type: "text/plain" }));
      return data;
    });
    await a.page.dispatchEvent("#dropzone", "drop", { dataTransfer: many });
    await expect(a.page.locator(".offer-card")).toHaveCount(2, { timeout: 30_000 });

    // The other peer sees both offers, and NOTHING started anywhere: a drop
    // publishes, it never transfers (D3).
    await expect(b.page.locator(".offer-card")).toHaveCount(2, { timeout: 30_000 });
    for (const peer of [a, b]) {
      const counters = await hookCounters(peer.page);
      expect(counters.outboundTypes).not.toContain("transfer.request");
      expect(counters.transferRows).toBe(0);
      expect(counters.rtc).toBe(0);
    }
    expect((await hookCounters(a.page)).outboundTypes.filter((t) => t === "offer.publish").length).toBe(2);
    // And the empty state went away exactly when the first offer arrived.
    expect(await b.page.locator("#catalog-empty").count()).toBe(0);
    expect(await b.page.locator("#transfers-empty").count()).toBe(1);

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
    }
    // Re-resolve on every pass: a withdrawal re-renders the catalog, so a
    // handle collected before the first click is detached by the second and
    // `click()` then waits for it forever.
    for (let left = await a.page.locator("[data-withdraw]").count(); left > 0; left -= 1) {
      await a.page.locator("[data-withdraw]").first().click({ timeout: 15_000 });
      await expect(a.page.locator("[data-withdraw]")).toHaveCount(left - 1, {
        timeout: 20_000,
      });
    }
    await expect(b.page.locator(".offer-card")).toHaveCount(0, { timeout: 20_000 });
    await a.cleanup();
    await b.cleanup();
  });

  test("T-WEB-PATH-UI the badge says only what a verified chunk proved", async () => {
    test.setTimeout(420_000);
    const a = await openPeer();
    // This recipient has no WebRTC at all, so the transfer goes to the relay
    // at once: the badge must still read `in connessione` first and only
    // then `relay`, because a committed path that has carried nothing is not
    // a fact yet.
    const relayOnly = await openPeer({ noWebRtc: true });
    await expectConnected(a.page);
    await expectConnected(relayOnly.page);
    expect(await opfsWorks(relayOnly.page)).toBe(true);
    const offer = await publish(a, "grande.bin");
    await expect(relayOnly.page.locator(`button[data-download="${offer}"]`)).toBeVisible({
      timeout: 30_000,
    });
    await watchPath(relayOnly.page);
    await relayOnly.page.locator(`button[data-download="${offer}"]`).click();
    // The word, not only the attribute: this is what the user reads.
    await expect(relayOnly.page.locator(".transfer-path .path-word")).toHaveText(
      "in connessione",
      { timeout: 30_000 },
    );
    await expect(relayOnly.page.locator(".transfer-path")).toHaveAttribute(
      "data-path",
      "relay",
      { timeout: 240_000 },
    );
    await expect(relayOnly.page.locator(".transfer-path .path-word")).toHaveText("relay");
    // A shape as well as a word, and a line that says what it means.
    await expect(relayOnly.page.locator(".transfer-path .path-mark")).toHaveText("▲");
    expect(
      await relayOnly.page.locator(".transfer-path").getAttribute("title"),
    ).toContain("server");
    expect(await pathTrail(relayOnly.page)).toEqual(["connecting", "relay"]);
    await expect(relayOnly.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    await relayOnly.page.locator("#discard-file").click();
    await relayOnly.cleanup();

    // Now the transition that matters: a DIRECT leg that dies mid-transfer.
    // The badge must go BACK to `in connessione` before it says `relay` —
    // the direct transport it named is gone, and the replacement has proved
    // nothing yet.
    //
    // The scenario needs the direct leg to carry a verified chunk first, and
    // that is a PREFERENCE of the product, not a guarantee (§8.55): under the
    // full suite on three engines a peer legitimately lands straight on the
    // relay, and an attempt that did so has not exercised this claim at all.
    // The killer records `killedAt` exactly when it fired, so the attempt can
    // say whether the scenario happened instead of the assertion guessing.
    // Two attempts; a direct leg that never comes up twice is a real finding.
    let trail = null;
    for (let attempt = 1; attempt <= 2 && trail === null; attempt += 1) {
      const falling = await openPeer({ init: killChannelAfter(2 * CHUNK_BYTES) });
      await expectConnected(falling.page);
      expect(await opfsWorks(falling.page)).toBe(true);
      await expect(falling.page.locator(`button[data-download="${offer}"]`)).toBeVisible({
        timeout: 30_000,
      });
      await watchPath(falling.page);
      await falling.page.locator(`button[data-download="${offer}"]`).click();
      // The badge reaching `relay` is the whole of this claim, and it is
      // reached long before the last byte: waiting for the file to finish
      // would re-prove a recovery `T-WEB-DIRECT-FALLBACK` already owns, and
      // that tail is what stalled this gate once under full-suite load.
      await expect(falling.page.locator(".transfer-path")).toHaveAttribute(
        "data-path",
        "relay",
        { timeout: 240_000 },
      );
      const seen = await pathTrail(falling.page);
      const killed = await falling.page.evaluate(
        () => window.__BORE_TEST__.killedAt ?? null,
      );
      // True of EVERY attempt, direct or not: the badge never opens on a
      // transport it has not verified.
      expect(seen[0]).toBe("connecting");
      expect(seen.at(-1)).toBe("relay");
      if (killed !== null) {
        trail = seen;
      }
      // Leave nothing running behind: the transfer is cancelled, not finished.
      const fallingId = await falling.page.evaluate(
        () => window.__BORE_TEST__.receiverState()[0]?.transferId ?? null,
      );
      if (fallingId !== null) {
        await falling.page.locator(`button[data-cancel="${fallingId}"]`).click();
        await expect(
          falling.page.locator(`.transfer-row[data-transfer="${fallingId}"] .transfer-state`),
        ).toContainText("Annullato", { timeout: 30_000 });
      }
      await falling.cleanup();
    }
    expect(trail, "the direct leg never carried a verified chunk in two attempts").not.toBeNull();
    // The kill only fires once the badge has NAMED the direct leg, so the
    // trail must carry it.
    expect(trail).toContain("direct");
    // And between them it went back to `connecting`: no gap where the badge
    // named a dead transport.
    expect(trail.slice(trail.indexOf("direct"))).toContain("connecting");
    expect(trail.indexOf("connecting", trail.indexOf("direct"))).toBeLessThan(
      trail.lastIndexOf("relay"),
    );
    // The SOURCE writes into a channel the recipient has just killed. WebKit
    // reports that write as a page-level error ("Error sending binary data
    // through RTCDataChannel.") which no JS can catch: the channel still
    // reads `open` when `send` is called and the transport is already gone,
    // so the sink's own guard and its try/catch both see nothing. It is the
    // engine describing the transport the test deliberately destroyed, not
    // the app failing — every OTHER failure still counts, and the app's own
    // answer to that transport is what the trail above already proved.
    expect(a.failures.filter((line) => !/RTCDataChannel/.test(line))).toEqual([]);
    await a.cleanup();
  });

  test("T-WEB-UI the layout holds still, the cancel stays reachable, tab order is the visual order", async () => {
    test.setTimeout(420_000);
    const a = await openPeer();
    const b = await openPeer({ noWebRtc: true });
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    // Focus is visible by rule, not by luck: the stylesheet the page
    // actually loaded carries a `:focus-visible` outline for its controls.
    const focusRule = await b.page.evaluate(() => {
      for (const sheet of document.styleSheets) {
        let rules;
        try {
          rules = sheet.cssRules;
        } catch {
          continue;
        }
        for (const rule of rules) {
          if (
            rule.selectorText?.includes(":focus-visible") &&
            /outline/.test(rule.style?.cssText ?? "")
          ) {
            return rule.selectorText;
          }
        }
      }
      return null;
    });
    expect(focusRule).not.toBeNull();

    // Tab order is the DOM order, and the DOM order is the visual order.
    // Nothing raises itself with a positive tabindex, so the engine's own
    // sequential order IS the order below.
    const order = await b.page.evaluate(() => {
      const focusable = [...document.querySelectorAll("button, input, summary, [tabindex]")].filter(
        (node) =>
          node.offsetParent !== null &&
          node.getAttribute("aria-hidden") !== "true" &&
          node.getAttribute("tabindex") !== "-1",
      );
      return focusable.map((node) => {
        const box = node.getBoundingClientRect();
        return {
          id: node.id || node.className || node.tagName,
          top: Math.round(box.top),
          bottom: Math.round(box.bottom),
          left: Math.round(box.left),
          tabindex: node.getAttribute("tabindex"),
        };
      });
    });
    expect(order.length).toBeGreaterThan(3);
    for (const node of order) {
      expect(node.tabindex === null || Number(node.tabindex) <= 0).toBe(true);
    }
    for (let i = 1; i < order.length; i += 1) {
      const before = order[i - 1];
      const now = order[i];
      // "Same line" is a vertical OVERLAP, not an equal top: a centred row
      // holding a tall control beside a short one gives the tall one the
      // smaller top, and comparing tops alone reads that as a jump upwards.
      const sameLine = now.top < before.bottom && before.top < now.bottom;
      if (sameLine) {
        // On one line, never right to left.
        expect(now.left).toBeGreaterThanOrEqual(before.left - 1);
      } else {
        // Between lines, never upward.
        expect(now.top).toBeGreaterThanOrEqual(before.top);
      }
    }

    const offer = await publish(a, "grande.bin");
    await expect(b.page.locator(`button[data-download="${offer}"]`)).toBeVisible({
      timeout: 30_000,
    });
    const boxesBefore = await zoneBoxes(b.page);

    await b.page.locator(`button[data-download="${offer}"]`).click();
    const transferId = await poll(b.page, () => {
      const rows = window.__BORE_TEST__.receiverState();
      return rows.length === 1 ? rows[0].transferId : null;
    });
    const cancel = b.page.locator(`button[data-cancel="${transferId}"]`);
    await expect(cancel).toBeVisible({ timeout: 60_000 });
    // The cancel says what it costs before it is used.
    expect(await cancel.getAttribute("title")).toContain("entrambi");

    // Two samples several progress ticks apart: the zones above the transfer
    // do not move, and the cancel button stays exactly where it was and
    // stays clickable.
    const cancelBefore = await cancel.boundingBox();
    const scrollBefore = await b.page.evaluate(() => Math.round(window.scrollY));
    await expect(async () => {
      const value = await b.page
        .locator(`.transfer-row[data-transfer="${transferId}"] progress`)
        .getAttribute("value");
      expect(Number(value)).toBeGreaterThanOrEqual(10);
    }).toPass({ timeout: 240_000 });
    const boxesAfter = await zoneBoxes(b.page);
    expect(boxesAfter).toEqual(boxesBefore);
    // And the page did not scroll under the user either: the boxes above are
    // measured against the DOCUMENT, so a scroll would pass them silently.
    expect(await b.page.evaluate(() => Math.round(window.scrollY))).toBe(scrollBefore);
    const cancelAfter = await cancel.boundingBox();
    expect(Math.round(cancelAfter.y)).toBe(Math.round(cancelBefore.y));
    await expect(cancel).toBeEnabled();

    // The ARIA value tracks the bar, live.
    const aria = await b.page.evaluate((id) => {
      const bar = document.querySelector(`.transfer-row[data-transfer="${id}"] progress`);
      return {
        role: bar.getAttribute("role"),
        now: bar.getAttribute("aria-valuenow"),
        value: bar.getAttribute("value"),
        min: bar.getAttribute("aria-valuemin"),
        max: bar.getAttribute("aria-valuemax"),
      };
    }, transferId);
    expect(aria.role).toBe("progressbar");
    expect(aria.now).toBe(aria.value);
    expect(aria.min).toBe("0");
    expect(aria.max).toBe("100");

    // And the click still lands, which is the whole point of not rebuilding
    // the zone under the user.
    await cancel.click();
    await expect(
      b.page.locator(`.transfer-row[data-transfer="${transferId}"] .transfer-state`),
    ).toContainText("Annullato");
    expect(a.failures).toEqual([]);
    expect(b.failures).toEqual([]);
    await a.cleanup();
    await b.cleanup();
  });
});
