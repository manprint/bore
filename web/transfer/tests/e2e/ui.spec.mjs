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
      // ACROSS CARRIERS, both the count and the kill. A direct attempt is
      // several peer connections now (one per carrier, see
      // --web-transfer-direct-carriers), each with its own channel, and the
      // attempt dies only when the LAST one is gone: a per-channel counter
      // both fired late (each carrier sees its own share of the bytes) and
      // killed one carrier of N, which the product correctly survives -- so
      // the transition this gate exists for never happened and the gate timed
      // out instead of failing. NO BACKTICK in here: template literal.
      const shared = (window.__BORE_KILL__ = window.__BORE_KILL__ || {
        seen: 0,
        channels: [],
      });
      shared.channels.push(channel);
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
        shared.seen += data && data.byteLength !== undefined ? data.byteLength : (data && data.size) || 0;
        if (shared.seen >= LIMIT && named()) {
          try { window.__BORE_TEST__.killedAt = shared.seen; } catch {}
          for (const open of shared.channels) {
            try { open.close(); } catch {}
          }
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
    // The word, not only the attribute: this is what the user reads. The
    // relay can verify its first chunk before Playwright's next sample, so
    // observe the transition trace rather than requiring an ephemeral text
    // value to remain visible for a whole polling interval.
    await expect
      .poll(
        async () => (await pathTrail(relayOnly.page)).includes("connecting"),
        { timeout: 30_000 },
      )
      .toBe(true);
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
    const attempts = [];
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
      // A SOFT wait, not an assertion: an assertion here aborts the loop on
      // the first attempt, which makes the two attempts above decorative. The
      // whole reason there are two is that what this gate needs — a direct
      // leg that comes up, carries a verified chunk and then dies — is a
      // preference of the product and not a guarantee (§8.55). On the ubuntu
      // CI runner, two cores serving three engines, attempt 1 spent the whole
      // budget at `connecting` and took the gate down with it while attempt 2
      // was never allowed to run.
      // 90 s, not 240: TWO attempts have to fit inside this test's own
      // 420 s budget together with the first half of the gate. At 240 s each
      // a product that never falls back makes the gate TIME OUT instead of
      // failing with the message below — measured, and the timeout says
      // nothing about which side did not move.
      const reached = await falling.page
        .waitForFunction(
          () => document.querySelector(".transfer-path")?.dataset.path === "relay",
          null,
          { timeout: 90_000 },
        )
        .then(() => true)
        .catch(() => false);
      const seen = await pathTrail(falling.page);
      const killed = await falling.page.evaluate(
        () => window.__BORE_TEST__.killedAt ?? null,
      );
      // True of EVERY attempt, direct or not: the badge never opens on a
      // transport it has not verified.
      expect(seen[0]).toBe("connecting");
      attempts.push({
        attempt,
        reached,
        killed,
        seen,
        // Both sides' own account, because a fallback that does not happen is
        // one side not moving and the badge cannot say which.
        recipient: await falling.page.evaluate(() => ({
          direct: [...window.__BORE_TEST__.directEvents],
          commits: [...window.__BORE_TEST__.pathCommits],
        })),
        source: await a.page.evaluate(() => ({
          direct: [...window.__BORE_TEST__.directEvents],
          inbound: [...window.__BORE_TEST__.inboundMarks].map((m) =>
            m.type === "error" ? `error:${m.code}` : m.type,
          ),
        })),
      });
      if (reached && killed !== null) {
        expect(seen.at(-1)).toBe("relay");
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
    expect(
      trail,
      `no attempt showed a direct leg that carried a verified chunk and then fell back: ${JSON.stringify(attempts)}`,
    ).not.toBeNull();
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

  // T-WEB-UI-RESPONSIVE (T-A018). A layout is responsive when it survives the
  // narrowest screen people actually use AND the content the page does not
  // choose: a file name arrives from another peer and can be any length, and
  // an unbreakable one is the single commonest way a page like this starts
  // scrolling sideways. So the gate publishes a 180-character name and then
  // asks the ENGINE whether the document is wider than the window — the
  // measurement a screenshot cannot make and an eye misses.
  test("T-WEB-UI-RESPONSIVE a phone screen fits, with a name the page did not choose", async () => {
    test.setTimeout(180_000);
    // No hyphen, no underscore, no dot until the extension: an engine breaks
    // a line at those, so a name that HAS them is not the hard case. This one
    // offers the layout no break opportunity at all, which is what makes the
    // gate red without `overflow-wrap`.
    // 121 characters: just under `MAX_LABEL_CHARS` (128), which is the
    // longest name the product itself accepts — a longer one is refused at
    // publish and would measure the validator instead of the layout.
    const longName = `${"Registrazione".repeat(9)}.bin`;
    writeFileSync(join(roomDir, longName), filler(4096, 9));

    const peer = await openPeer();
    await expectConnected(peer.page);
    await publish(peer, longName);

    // 320 is the narrowest phone still in use; 360 is the common one. The
    // claim has to hold at both, so both are measured rather than assumed
    // from one.
    for (const width of [320, 360, 414]) {
      await peer.page.setViewportSize({ width, height: 740 });
      const overflow = await peer.page.evaluate(() => ({
        doc: document.documentElement.scrollWidth,
        win: window.innerWidth,
        widest: [...document.querySelectorAll("#app *")]
          .map((node) => {
            const box = node.getBoundingClientRect();
            return [node.id || node.className || node.tagName, Math.round(box.right)];
          })
          .sort((a, b) => b[1] - a[1])[0],
      }));
      // One pixel of tolerance: a fractional layout rounds up, and a
      // sub-pixel is not a sideways scroll.
      expect(
        overflow.doc,
        `at ${width}px the document is ${overflow.doc}px wide; the widest box is ${JSON.stringify(overflow.widest)}`,
      ).toBeLessThanOrEqual(overflow.win + 1);
    }

    // The three zones stay ONE COLUMN and in order — the same property
    // `T-WEB-UI` asserts for the tab sequence, here at phone width where a
    // careless grid would have wrapped them side by side.
    await peer.page.setViewportSize({ width: 360, height: 740 });
    const zones = await peer.page.evaluate(() =>
      ["zone-room", "zone-offers", "zone-transfers"].map((id) => {
        const box = document.getElementById(id).getBoundingClientRect();
        return {
          id,
          top: Math.round(box.top + window.scrollY),
          left: Math.round(box.left),
          width: Math.round(box.width),
        };
      }),
    );
    for (let i = 1; i < zones.length; i += 1) {
      expect(zones[i].top).toBeGreaterThan(zones[i - 1].top);
      expect(zones[i].left).toBe(zones[0].left);
    }

    // Touch targets. A control under ~44px is one a thumb misses, and the
    // stylesheet claims `--tap`; this is what makes the claim true rather
    // than written. Measured on the controls a first-time user needs, not on
    // every button in the tree.
    const targets = await peer.page.evaluate(() =>
      ["add-file", "add-folder", "copy-link", "rename-input"]
        .map((id) => document.getElementById(id))
        .filter((node) => node !== null && node.offsetParent !== null)
        .map((node) => [node.id, Math.round(node.getBoundingClientRect().height)]),
    );
    expect(targets.length).toBeGreaterThan(3);
    for (const [id, height] of targets) {
      expect(height, `${id} is ${height}px tall`).toBeGreaterThanOrEqual(43);
    }

    expect(peer.failures).toEqual([]);
    await peer.cleanup();
  });

  // The other half of the same claim: a wide screen must not simply stretch.
  // An unbounded line of file names on a 27-inch monitor is as unusable as a
  // sideways scroll on a phone, and it is the failure that gets shipped
  // because the developer's own window hides it.
  test("T-WEB-UI-RESPONSIVE a wide screen keeps one column and a bounded measure", async () => {
    test.setTimeout(120_000);
    const peer = await openPeer();
    await expectConnected(peer.page);
    await peer.page.setViewportSize({ width: 1600, height: 900 });

    const layout = await peer.page.evaluate(() => {
      const main = document.getElementById("app");
      const box = main.getBoundingClientRect();
      return {
        width: Math.round(box.width),
        left: Math.round(box.left),
        right: Math.round(window.innerWidth - box.right),
        zones: ["zone-room", "zone-offers", "zone-transfers"].map((id) => {
          const z = document.getElementById(id).getBoundingClientRect();
          return { top: Math.round(z.top + window.scrollY), left: Math.round(z.left) };
        }),
      };
    });
    // Bounded: the measure is 56rem, so the container never spans 1600px.
    expect(layout.width).toBeLessThanOrEqual(900);
    // Centred, so the content is not pinned to one edge of a wide screen.
    expect(Math.abs(layout.left - layout.right)).toBeLessThanOrEqual(2);
    // Still one column, still in order.
    for (let i = 1; i < layout.zones.length; i += 1) {
      expect(layout.zones[i].top).toBeGreaterThan(layout.zones[i - 1].top);
      expect(layout.zones[i].left).toBe(layout.zones[0].left);
    }

    expect(peer.failures).toEqual([]);
    await peer.cleanup();
  });
});
