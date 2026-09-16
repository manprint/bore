// Shared browser instrumentation for the multipeer suites (2.6+).
//
// The app itself installs every recorder when `window.__BORE_TEST__` exists
// before app load (see src/main.js); this module only ensures the hook
// object exists and reads the counters back. Production runs without the
// hook: nothing is recorded, nothing changes.
export async function installTestHooks(context) {
  await context.addInitScript(() => {
    window.__BORE_TEST__ = window.__BORE_TEST__ ?? {};
  });
}

/**
 * Makes this context look like an engine WITHOUT WebRTC, before the app
 * loads. The page then declines the direct attempt as `unsupported` the
 * moment the server opens it, so the transfer falls back to the encrypted
 * relay at once instead of waiting out the 10 s deadline.
 *
 * This is a real supported case, not a test shortcut: a browser with WebRTC
 * disabled by policy behaves exactly like this, and every relay gate written
 * before 4.2 is a gate on THAT case now that direct is the default.
 */
export async function disableWebRtc(context) {
  await context.addInitScript(() => {
    for (const name of ["RTCPeerConnection", "webkitRTCPeerConnection", "mozRTCPeerConnection"]) {
      try {
        Object.defineProperty(window, name, { configurable: true, value: undefined });
      } catch {
        /* an engine that never had it needs nothing */
      }
    }
  });
}

/**
 * Replaces `win.RTCPeerConnection` with a wrapper that changes EXACTLY one
 * thing: every configuration gets `iceTransportPolicy: "relay"`. With no
 * TURN server configured — the room never offers one, only STUN — ICE then
 * finds no candidate pair and the direct attempt fails on its own, which is
 * a real supported case and not a test shortcut.
 *
 * Everything else about the API is preserved: the prototype (so `instanceof`
 * still answers), every own static, and every other configuration field the
 * caller passed. Defaulted to `globalThis` so the SAME function body can be
 * handed to Playwright's `addInitScript` (which serializes it and calls it
 * with no argument in the page) and called directly against a fake window by
 * the unit test — one implementation, two callers, no second copy to drift.
 *
 * @param {object} [win] window-like object holding `RTCPeerConnection`
 */
export function applyRelayOnlyPolicy(win = globalThis) {
  const Real = win.RTCPeerConnection;
  if (typeof Real !== "function") {
    return;
  }
  const Wrapped = function RTCPeerConnection(config, ...rest) {
    return new Real({ ...(config ?? {}), iceTransportPolicy: "relay" }, ...rest);
  };
  Wrapped.prototype = Real.prototype;
  for (const key of Object.getOwnPropertyNames(Real)) {
    if (["length", "name", "prototype"].includes(key)) {
      continue;
    }
    try {
      Wrapped[key] = Real[key];
    } catch {
      /* a non-writable static stays the real one */
    }
  }
  win.RTCPeerConnection = Wrapped;
}

/**
 * Keeps WebRTC present but forces every connection to `relay` with no TURN
 * server configured, so ICE genuinely finds no candidate pair and the direct
 * attempt fails on its own. See {@link applyRelayOnlyPolicy}, which is the
 * body that runs in the page and is unit-tested on its own.
 */
export async function forceIceRelayOnly(context) {
  await context.addInitScript(applyRelayOnlyPolicy);
}

/** Snapshot of the app-side recorders (all zero/absent without the hook). */
export async function hookCounters(page) {
  return page.evaluate(() => {
    const hook = window.__BORE_TEST__ ?? {};
    return {
      outboundTypes: [...(hook.outboundTypes ?? [])],
      wsUrls: [...(hook.wsUrls ?? [])],
      fileReads: (hook.fileReads ?? []).length,
      rtc: hook.rtcConstructed ?? 0,
      transferRows: typeof hook.transferRows === "function" ? hook.transferRows() : 0,
      resumeRequests: [...(hook.resumeRequests ?? [])],
    };
  });
}

/** Live catalog through the app hook (offerId/peerId/manifest/mac). */
export async function hookCatalog(page) {
  return page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot());
}
