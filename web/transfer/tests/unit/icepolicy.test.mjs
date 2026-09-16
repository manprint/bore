// 4.4 — the ICE wrapper the multipeer scenario installs on peer C.
//
// The scenario's whole claim rests on this object: C must be the SHIPPED
// page, using the SHIPPED WebRTC code, failing for the reason a real
// restricted network fails — no candidate pair. A wrapper that changed
// anything else would turn `T-WEB-MULTIPEER` into a test of the wrapper.
// So the wrapper is pinned here, on its own, against a fake engine.
import test from "node:test";
import assert from "node:assert/strict";
import { applyRelayOnlyPolicy } from "../e2e/fixtures.js";

/** A fake engine with the shape the real constructor has. */
function fakeEngine() {
  const seen = [];
  class FakeRTCPeerConnection {
    constructor(config, extra) {
      seen.push({ config, extra });
      this.config = config;
      this.extra = extra;
    }
    createDataChannel(label) {
      return { label };
    }
  }
  // Statics an engine really carries: one ordinary, one that refuses to be
  // read at all (the case the copy loop's `catch` exists for).
  FakeRTCPeerConnection.generateCertificate = () => "cert";
  Object.defineProperty(FakeRTCPeerConnection, "hostileStatic", {
    get() {
      throw new TypeError("illegal invocation");
    },
    configurable: false,
  });
  return { FakeRTCPeerConnection, seen };
}

test("rtc_policy_wrapper_preserves_api_and_forces_relay_only", () => {
  const { FakeRTCPeerConnection: Real, seen } = fakeEngine();
  const win = { RTCPeerConnection: Real };
  applyRelayOnlyPolicy(win);
  const Wrapped = win.RTCPeerConnection;

  // It IS a replacement, and it keeps the name the page reads in a stack.
  assert.notEqual(Wrapped, Real);
  assert.equal(Wrapped.name, "RTCPeerConnection");
  // The prototype is the real one, so `instanceof` and every prototype
  // method the app calls still resolve to the engine's own.
  assert.equal(Wrapped.prototype, Real.prototype);
  // Statics survive, and a static that throws when read is skipped rather
  // than failing the installation — the page must come up either way.
  assert.equal(Wrapped.generateCertificate, Real.generateCertificate);
  assert.equal(Wrapped.hostileStatic, undefined);

  // The ONLY field it changes is the policy; everything else the caller
  // passed reaches the engine untouched, extra arguments included.
  const conn = new Wrapped(
    { iceServers: [{ urls: "stun:example:3478" }], bundlePolicy: "max-bundle" },
    { certificates: [] },
  );
  assert.ok(conn instanceof Real);
  assert.equal(seen.length, 1);
  assert.deepEqual(seen[0].config, {
    iceServers: [{ urls: "stun:example:3478" }],
    bundlePolicy: "max-bundle",
    iceTransportPolicy: "relay",
  });
  assert.deepEqual(seen[0].extra, { certificates: [] });
  // A caller-supplied policy is OVERRIDDEN, not merged: relay-only is the
  // point of the wrapper and the page must not be able to opt out of it.
  new Wrapped({ iceTransportPolicy: "all" });
  assert.equal(seen[1].config.iceTransportPolicy, "relay");
  // No configuration at all is still a configured relay-only connection.
  new Wrapped();
  assert.deepEqual(seen[2].config, { iceTransportPolicy: "relay" });
  // The instance the app gets is the engine's own object, so the channel it
  // creates comes from the real prototype.
  assert.deepEqual(conn.createDataChannel("bore"), { label: "bore" });

  // An engine WITHOUT WebRTC is left exactly as it was: the wrapper must
  // never manufacture a constructor that does not exist.
  const bare = { RTCPeerConnection: undefined };
  applyRelayOnlyPolicy(bare);
  assert.equal(bare.RTCPeerConnection, undefined);
});

test("relay_only_policy_source_is_self_contained_for_addinitscript", () => {
  // Playwright serializes the function and runs it in the page with no
  // argument, so it may not close over anything from this module and its
  // default must be the page's own global. Both are properties of the
  // SOURCE, which is what is actually shipped into the browser.
  const source = applyRelayOnlyPolicy.toString();
  assert.match(source, /win = globalThis/);
  assert.equal(/\bimport\b|\brequire\(/.test(source), false);
});
