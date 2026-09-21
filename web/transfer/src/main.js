import "./styles.css";
import { buildShortRoomUrl, parseShortRoomUrl } from "./secrets.js";
import {
  CONNECTION,
  TRANSFER,
  createInitialState,
  errorText,
  messageToEvent,
  partitionRepublish,
  reduce,
} from "./state.js";
import { createControlSession } from "./control.js";
import { createOfferManager } from "./offers.js";
import { createReceiver } from "./receiver.js";
import { createSender } from "./sender.js";
import { createRepository, sanitizeDownloadName } from "./storage.js";
import { MAX_CARRIERS, createCarrierGroup } from "./webrtc.js";
import { createTraceStore } from "./diagnostics.js";
import { createView } from "./view.js";
import {
  bytesToHex,
  deriveRoomLinkMaterial,
  hexToBytes,
  manifestMac,
} from "./crypto.js";
import { canonicalize, directFailedBody, manifestValue } from "./protocol.js";

// Browser bootstrap: derive room credentials from the persistent short-link
// fragment in memory, then open one control session and peer/catalog
// rendering, local offer preparation (hash in a worker, publish on the
// control channel), the source transfer actor (auto-ready on incoming,
// relay leg on ticket, payload pipeline on path_commit) and the recipient
// actor behind the ONE click path below.
//
// Two rules this file exists to keep: `transfer.request` is constructed in
// `receiver.js` alone and reached only from `onDownload` (a click handler),
// and a reconnect republishes local offers but never resumes a download —
// a partial shows as "Riprendi" and waits for another click.

const app = document.getElementById("app");
let state = createInitialState();
let secrets = null;
let roomId = null;
let roomSeed = null;
let session = null;
let serverLimits = null;
let offers = null;
let sender = null;
let receiver = null;
/** Own peer ID from the `welcome` body (relay attach identity). */
let selfPeerId = null;
/** Republish candidates waiting out their ghost session's removal. */
const deferredRepublish = new Set();

/** Request ID for the few control messages this file sends itself. */
function randomRequestId() {
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Local speed estimate per transfer (never sent, never logged). */
const speeds = new Map();

function trackSpeed(transferId, doneBytes) {
  const now = Date.now();
  const previous = speeds.get(transferId);
  if (previous === undefined) {
    speeds.set(transferId, { at: now, bytes: doneBytes, bps: null });
    return null;
  }
  const elapsed = now - previous.at;
  if (elapsed < 500) {
    return previous.bps;
  }
  const bps = Math.max(0, ((doneBytes - previous.bytes) * 1000) / elapsed);
  speeds.set(transferId, { at: now, bytes: doneBytes, bps });
  return bps;
}

function applyTransferEvent(event) {
  state = reduce(state, event);
  render();
}

/** Re-reads which offers have a partial on disk (resume is click-only). */
function refreshResumable() {
  if (receiver === null) {
    return;
  }
  receiver
    .resumableOffers()
    .then((offerIds) => {
      applyTransferEvent({ kind: "transfer.resumable", offerIds });
    })
    .catch(() => {
      /* a storage probe that fails just leaves the markers as they are */
    });
}

function makeOffers() {
  return createOfferManager({
    createWorker: () =>
      new Worker("/transfer/assets/offer-worker.js", { type: "module" }),
    roomIdHex: roomId,
    roomKeyHex: secrets.roomKey,
    sendControl: (message) => {
      if (session === null) {
        return false;
      }
      return session.send(
        message.type,
        message.requestId ?? null,
        message.body,
      );
    },
    events: {
      onProgress: () => renderOffers(),
      onDone: () => {
        renderOffers();
        view.announce("Offerta pronta");
      },
      onError: (_offerId, message) => {
        renderOffers();
        view.announce(`Offerta non riuscita: ${message}`);
      },
      onWithdrawn: () => renderOffers(),
    },
  });
}

function offersUi() {
  const ui = new Map();
  if (offers === null) {
    return ui;
  }
  for (const [offerId, record] of offers.offers()) {
    ui.set(offerId, {
      status: record.status,
      progress01: record.progress ?? 0,
    });
  }
  return ui;
}

function renderOffers() {
  view.render(state, offersUi());
}

const view = createView(document, app, {
  onRename: (name) => {
    if (session === null || !session.rename(name)) {
      view.announce("Non connesso: nome non inviato");
    }
  },
  // Every way a SELECTION can fail — the directory picker throwing, a drop
  // the engine will not expose, a drop refused because the room is gone —
  // arrives here. It used to arrive nowhere: `onSelectionError` was never
  // wired, so `callbacks.onSelectionError?.()` in the view was a no-op and a
  // failed drop produced silence (5.6).
  onSelectionError: (message) => {
    view.announce(`Selezione non riuscita: ${message}`);
  },
  onSelectFiles: (files, origin) => {
    if (offers === null) {
      return;
    }
    // One picker/drop action is one offer: the kind follows the selection
    // (one file → file, several flat files → files, folder button → folder),
    // never the button that opened the picker.
    const kind =
      origin === "folder" ? "folder" : files.length > 1 ? "files" : "file";
    const result = offers.prepareSelection({ kind, files });
    if (result.error) {
      view.announce(result.error);
      return;
    }
    renderOffers();
  },
  onWithdraw: (offerId) => {
    if (offers === null) {
      return;
    }
    const result = offers.withdrawOffer(offerId);
    if (result.error) {
      view.announce(result.error);
      return;
    }
    renderOffers();
  },
  onDownload: (offerId, mode = "raw", entryId = null) => {
    // The ONLY download entry point: one click, one `transfer.request`.
    // `entryId` names ONE file of a folder offer; without it a `raw`
    // download means the offer's single file.
    void startDownload(offerId, mode, { entryId });
  },
  onRestartDownload: (offerId, mode = "raw", entryId = null) => {
    // The second gesture after a `SOURCE_CHANGED`: it is the only path in
    // the app that throws away bytes this peer verified, so it exists as
    // its own button and never as a silent retry.
    void startDownload(offerId, mode, { entryId, fresh: true });
  },
  onCancelTransfer: (transferId) => {
    // Abort first (inside the actors), then the idempotent control message.
    closeDirect(transferId, "cancelled-here");
    const cancelled = receiver?.cancelTransfer(transferId) === true;
    const aborted = sender?.abortTransfer(transferId) === true;
    if (aborted && !cancelled) {
      session?.send("transfer.cancel", randomRequestId(), { transferId });
    }
    applyTransferEvent({
      kind: "transfer.state",
      transferId,
      state: TRANSFER.CANCELLED,
      code: "CANCELLED",
    });
    view.announce("Trasferimento annullato");
    refreshResumable();
  },
  onCopyLink: async () => {
    if (roomSeed === null) {
      return;
    }
    const url = buildShortRoomUrl(window.location.origin, roomSeed);
    try {
      await navigator.clipboard.writeText(url);
      view.announce("Link copiato negli appunti");
    } catch {
      view.announce("Copia non riuscita");
    }
  },
  /**
   * The direct-path diagnostic (V003-C3). It exists because a fallback used
   * to leave no evidence at all: the page abandoned the attempt, the relay
   * took over and nobody — user, operator or this repository — could say
   * which path had been selected or what ended it.
   *
   * It leaves the page ONLY here, on this click, and it carries no address,
   * no candidate line, no SDP, no file name, no peer name and no secret:
   * `diagnostics.js` copies numbers and short enumerations and nothing else.
   */
  onCopyDiagnostics: async () => {
    const text = directTraces.text({
      live: [...directAttempts.values()].flatMap((entry) =>
        entry.actor.diagnostics(),
      ),
    });
    try {
      await navigator.clipboard.writeText(text);
      view.announce(
        directTraces.size === 0
          ? "Diagnostica copiata (nessun tentativo diretto ancora concluso)"
          : "Diagnostica del percorso copiata negli appunti",
      );
    } catch {
      view.announce("Copia non riuscita");
    }
  },
});

function render() {
  view.render(state, offersUi());
}

function setConnection(connection, statusText) {
  state = reduce(state, { kind: "connection", connection, statusText });
  render();
}

function teardownUnavailable(statusText) {
  if (session !== null) {
    session.stop();
    session = null;
  }
  revokeSpentAnchorUrls();
  roomSeed = null;
  roomId = null;
  secrets = null;
  setConnection(CONNECTION.UNAVAILABLE, statusText);
  view.announce(statusText);
}

function controlUrl() {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}/transfer/ws/control/${roomId}`;
}

function relayBase() {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}`;
}

/**
 * Verifies one announced offer's manifest tag and removes it from the
 * catalog when it does not authenticate. Asynchronous by necessity
 * (WebCrypto), so the removal lands one turn later and `startDownload`
 * carries the blocking check.
 */
function dropUnauthenticOffer(event) {
  const { offerId, peerId, manifest, mac } = event;
  const expected = (async () => {
    if (
      typeof mac !== "string" ||
      !/^[0-9a-f]{64}$/.test(mac) ||
      manifest === null
    ) {
      return null;
    }
    const canonical = new TextEncoder().encode(
      canonicalize(manifestValue(manifest)),
    );
    return bytesToHex(
      await manifestMac(
        hexToBytes(secrets.roomKey),
        hexToBytes(roomId),
        canonical,
      ),
    );
  })();
  expected
    .catch(() => null)
    .then((tag) => {
      if (tag === mac) {
        return;
      }
      state = reduce(state, { kind: "offer.removed", peerId, offerId });
      view.announce(errorText("MANIFEST_MAC"));
      const hook = globalThis.__BORE_TEST__;
      if (hook && Array.isArray(hook.controlErrors)) {
        hook.controlErrors.push({ code: "MANIFEST_MAC", offerId });
      }
      render();
    });
}

/**
 * Starts one download from a click. Resolves the offer in the live catalog,
 * refuses our own, and hands the rest to the receiver — which owns the only
 * `transfer.request` construction site in the app.
 */
async function startDownload(offerId, mode = "raw", options = {}) {
  const offer = state.offers.get(offerId);
  if (offer === undefined || receiver === null) {
    view.announce(errorText("OFFER_NOT_FOUND"));
    return { error: "OFFER_NOT_FOUND" };
  }
  if (offer.peerId === selfPeerId) {
    // Unreachable through the UI (own offers have no button) and refused
    // here too, so a stray call cannot request from ourselves.
    return { error: "OWN_OFFER" };
  }
  if (options.fresh === true) {
    applyTransferEvent({
      kind: "transfer.source_changed",
      offerId,
      changed: false,
    });
  }
  const result = await receiver.startDownload({
    offerId,
    manifest: offer.manifest,
    macHex: offer.mac,
    sourcePeerId: offer.peerId,
    mode,
    entryId: options.entryId ?? null,
    fresh: options.fresh === true,
  });
  if (result?.error) {
    view.announce(errorText(result.error));
  }
  return result;
}

// ---------------------------------------------------------------------------
// Direct attempts (4.2)
//
// One `transfer.direct_start` opens one attempt actor, keyed by TransferId.
// The actor owns the RTCPeerConnection and the single DataChannel; this file
// owns which transfer it belongs to, which control messages reach it and
// when it dies. Every message it produces names the attempt, and an actor
// whose attempt is no longer current is closed before a new one is built —
// so a late candidate or a late failure can never touch the live attempt.
// ---------------------------------------------------------------------------

/** transferId → `{ actor, attemptId, recipient }` for the live attempt. */
const directAttempts = new Map();

/**
 * The last few finished attempts' traces (V003-C3), bounded by the store.
 * Nothing reads it but the copy button and the test hook: it is evidence
 * held for the user, never telemetry.
 */
const directTraces = createTraceStore();

/** Drops the actor for this transfer, if any, without telling the server. */
function closeDirect(transferId, cause, code) {
  const entry = directAttempts.get(transferId);
  if (entry === undefined) {
    return;
  }
  directAttempts.delete(transferId);
  // `cause` (and the `code` that came with it, when one did) reach the
  // attempt's trace: every teardown used to leave the same bare `closed`
  // mark, so a diagnostic could not say whether the carriers went away
  // because the transfer ENDED, because the user cancelled, or because the
  // SERVER declared the attempt failed — the three look identical in the
  // trace and mean opposite things (B-A037).
  entry.actor.close(cause, code);
  // Read LAZILY: the attempt's last `getStats()` sample is issued inside
  // `close()` and lands a moment later, so a snapshot taken now would be the
  // one that is missing exactly the sample describing the failure.
  directTraces.push(() => entry.actor.diagnostics());
}

/**
 * Test-only reader for the traces above. It is a FUNCTION and not an array
 * because the store resolves its entries when it is read: the sample that
 * describes a failure lands after the attempt is dropped.
 */
function installDiagnosticsHook() {
  const hook = globalThis.__BORE_TEST__;
  if (hook === null || typeof hook !== "object") {
    return;
  }
  try {
    hook.readDirectDiagnostics = () => ({
      finished: directTraces.all(),
      live: [...directAttempts.values()].flatMap((entry) =>
        entry.actor.diagnostics(),
      ),
    });
  } catch {
    /* a frozen hook object costs the harness one reader, never the app */
  }
}

/** Closes every live attempt (room death, page teardown). */
function closeAllDirect() {
  for (const transferId of [...directAttempts.keys()]) {
    closeDirect(transferId, "room-gone");
  }
}

/** Test-only recorder: the direct attempt's lifecycle, never its SDP. */
function recordDirect(entry) {
  const hook = globalThis.__BORE_TEST__;
  if (hook && Array.isArray(hook.directEvents)) {
    try {
      hook.directEvents.push(entry);
    } catch {
      /* recording must never break dispatch */
    }
  }
}

function sendDirectFailed(
  transferId,
  attemptId,
  reason,
  recipient,
  verifiedRanges,
) {
  recordDirect({
    kind: "failed",
    transferId,
    attemptId,
    recipient,
    reason,
    ranges: verifiedRanges ?? [],
  });
  // Only the recipient knows what it verified and wrote, and only it is
  // believed: these ranges are what the relay attempt will skip. The wire
  // name lives in `protocol.js` beside every other one.
  const body = directFailedBody(transferId, attemptId, reason, verifiedRanges);
  session?.send("transfer.direct_failed", randomRequestId(), body);
}

/**
 * Ends one direct attempt on OUR initiative and tells the server, unless the
 * transfer actor says the attempt is already over — a channel closing after
 * the last byte is the end of a successful transfer, not a failure, and a
 * notice there would cost a relay attempt nobody needs.
 */
async function failDirect(transferId, attemptId, reason, recipient, upgrade = false) {
  closeDirect(transferId, "failed-here", reason);
  if (upgrade) {
    // A probe that failed changes NOTHING: the relay never stopped carrying,
    // there is no path to reset and no ranges to report. Telling the server
    // is what lets it stop offering and try again later on its own grid.
    if (recipient) {
      receiver?.abandonUpgrade(transferId, attemptId);
    } else {
      sender?.detachUpgrade(transferId, attemptId);
    }
    session?.send("transfer.direct_failed", randomRequestId(), {
      transferId,
      attemptId,
      reason,
    });
    recordDirect({ kind: "ended", transferId, attemptId, recipient, reason });
    return;
  }
  if (recipient) {
    const ranges =
      (await (receiver?.directFailed(transferId, attemptId) ?? null)) ?? null;
    if (ranges === null) {
      // The transfer is past this attempt (verified, staged or gone): the
      // channel closing is the END of a successful transfer, not a failure,
      // and a notice here would cost a relay attempt nobody needs.
      recordDirect({ kind: "ended", transferId, attemptId, recipient, reason });
      return;
    }
    applyTransferEvent({ kind: "transfer.path_reset", transferId });
    sendDirectFailed(transferId, attemptId, reason, recipient, ranges);
    return;
  }
  if (sender?.detachDirect(transferId, attemptId) !== true) {
    recordDirect({ kind: "ended", transferId, attemptId, recipient, reason });
    return;
  }
  applyTransferEvent({ kind: "transfer.path_reset", transferId });
  sendDirectFailed(transferId, attemptId, reason, recipient);
}

/**
 * The counterpart ended the attempt: drop our half. As the SOURCE there is
 * nothing to add — the server already knows and the relay ticket is on its
 * way. As the RECIPIENT there is: we hold the only true answer to "what is
 * already verified and on disk", and the counterpart's notice does not carry
 * it. Staying silent here made the replacement attempt re-send the whole
 * file whenever the SOURCE won the race to report the same dead channel,
 * which is decided by two engines' timers. The server adopts a LATE
 * recipient report for exactly this (4.3, `adopt_late_recipient_resume`) and
 * takes nothing from it but the ranges, so reporting is free and silence is
 * the only way to lose them.
 */
async function abandonDirect(body) {
  const { transferId, attemptId, reason } = body;
  if (typeof transferId !== "string") {
    return;
  }
  // The SERVER declared this attempt failed: the one teardown the page
  // did not decide, and the one the field report could not identify.
  closeDirect(
    transferId,
    "server-failed",
    typeof reason === "string" ? reason : undefined,
  );
  if (typeof attemptId !== "string") {
    return;
  }
  // The badge is reset ONLY where a failure is actually declared. It used to
  // be reset here, unconditionally, before either side was asked whether the
  // attempt had failed at all — and the counterpart's notice winning the
  // race is the ORDINARY case, not the exception. A channel that closes
  // after the last byte ends a SUCCESSFUL transfer, so resetting there made
  // the row stop naming the transport that had just carried the whole file.
  const ranges =
    (await (receiver?.directFailed(transferId, attemptId) ?? null)) ?? null;
  if (ranges !== null) {
    applyTransferEvent({ kind: "transfer.path_reset", transferId });
    sendDirectFailed(
      transferId,
      attemptId,
      typeof reason === "string" ? reason : "unknown",
      true,
      ranges,
    );
  }
  if (sender?.detachDirect(transferId, attemptId) === true) {
    applyTransferEvent({ kind: "transfer.path_reset", transferId });
  }
}

/** Routes one forwarded signalling message to the attempt it names. */
function routeSignal(type, body) {
  const entry = directAttempts.get(body.transferId);
  if (entry === undefined || entry.attemptId !== body.attemptId) {
    // A signal for an attempt we no longer hold is exactly what the server
    // acks and ignores on its side: silence is the whole handling.
    return;
  }
  void entry.actor.handleSignal(type, body);
}

/** Builds the attempt actor for one `transfer.direct_start`. */
function startDirectAttempt(body) {
  const { transferId, attemptId, role } = body;
  if (typeof transferId !== "string" || typeof attemptId !== "string") {
    return;
  }
  // Roles are the server's, never derived here: the recipient is the offerer
  // and the only side that creates the channel.
  const recipient = role === "offerer";
  // How many `RTCPeerConnection`s this attempt runs on. The server decides it
  // (`--web-transfer-direct-carriers`) and omits the field at 1, so a server
  // that predates carriers reads as one — which is what it can do.
  const carriers = Math.max(1, Math.min(Number(body.carriers ?? 1) || 1, MAX_CARRIERS));
  // An UPGRADE runs BESIDE a relay that is still carrying: the peers
  // negotiate a direct path without disturbing the one moving bytes, and the
  // server's `transfer.path_commit direct` is what switches them over. The
  // field is emitted only for a probe, so an ordinary first negotiation
  // reads it as absent and takes the path it always took.
  const upgrade = body.upgrade === true;
  closeDirect(transferId, "superseded");
  if (typeof globalThis.RTCPeerConnection !== "function") {
    // An engine without WebRTC (or a page that had it removed) says so at
    // once, so the relay attempt starts now instead of at the deadline.
    sendDirectFailed(transferId, attemptId, "unsupported", recipient);
    return;
  }
  // The recipient arms its reorder window for exactly this count: with more
  // than one carrier the arrival order is the order N independent
  // associations happened to deliver in, and the sequence in each frame's own
  // header is what puts the stream back together.
  if (upgrade) {
    const staged = recipient
      ? receiver?.prepareUpgrade(transferId, attemptId, carriers)
      : sender?.transfers().has(transferId);
    if (staged !== true) {
      // Nothing to upgrade here — the transfer finished, failed, or is not
      // on the relay after all. Declining is free: the relay is untouched.
      sendDirectFailed(transferId, attemptId, "protocol", recipient);
      return;
    }
  } else if (
    recipient &&
    receiver?.beginDirect(transferId, attemptId, carriers) !== true
  ) {
    sendDirectFailed(transferId, attemptId, "protocol", recipient);
    return;
  }
  const actor = createCarrierGroup({
    role,
    transferId,
    attemptId,
    carriers,
    iceServers: body.iceServers,
    sendSignal: (type, signalBody) =>
      session?.send(type, randomRequestId(), signalBody) ?? false,
    events: {
      onReady: (info) => {
        recordDirect({
          kind: "ready",
          transferId,
          attemptId,
          recipient,
          // The negotiated carrier count, so a gate can assert the number of
          // peer connections the page opened against what the SERVER asked
          // for instead of against a constant that the default may outgrow.
          carriers,
          upgrade,
          fragmentBytes: info?.fragmentBytes ?? null,
        });
        const attached = recipient
          ? true
          : upgrade
            ? sender?.attachUpgrade(transferId, attemptId, actor.sink) === true
            : sender?.attachDirect(transferId, attemptId, actor.sink) === true;
        if (!attached) {
          void failDirect(transferId, attemptId, "protocol", recipient, upgrade);
          return;
        }
        const readyBody = { transferId, attemptId };
        if (upgrade && recipient) {
          // Only the recipient knows what is verified on disk, and by now the
          // relay has delivered a great deal the server never counted: the
          // commit is built from THESE ranges, so without them the switch
          // would resend everything already on disk.
          const ranges = receiver?.upgradeRanges(transferId, attemptId);
          if (Array.isArray(ranges) && ranges.length > 0) {
            readyBody.resumeRanges = ranges;
          }
        }
        session?.send("transfer.direct_ready", randomRequestId(), readyBody);
      },
      onMessage: (data) => {
        if (recipient) {
          receiver?.deliverDirectFrame(transferId, attemptId, data);
        }
      },
      onFailed: (reason) => {
        void failDirect(transferId, attemptId, reason, recipient, upgrade);
      },
    },
  });
  directAttempts.set(transferId, { actor, attemptId, recipient });
  void actor.start();
}

/** Recipient transfer actor (3.4): explicit downloads into OPFS. */
function makeReceiver() {
  return createReceiver({
    sendControl: (message) => {
      if (session === null) {
        return false;
      }
      return session.send(
        message.type,
        message.requestId ?? null,
        message.body,
      );
    },
    createSocket: (url, protocol) => new WebSocket(url, protocol),
    relayBase: relayBase(),
    roomIdHex: roomId,
    roomKeyHex: secrets.roomKey,
    getSelfPeerId: () => selfPeerId,
    repository: createRepository(),
    events: {
      onStarted: (info) => {
        applyTransferEvent({
          kind: "transfer.started",
          transferId: info.transferId,
          offerId: info.offerId,
          direction: "in",
          sourcePeerId: info.sourcePeerId,
          recipientPeerId: selfPeerId,
          label: info.label,
          totalBytes: info.totalBytes,
        });
      },
      onProgress: (info) => {
        applyTransferEvent({
          kind: "transfer.progress",
          transferId: info.transferId,
          doneBytes: info.receivedBytes,
          totalBytes: info.totalBytes,
          bytesPerSecond: trackSpeed(info.transferId, info.receivedBytes),
        });
      },
      onPath: (transferId, path) => {
        applyTransferEvent({ kind: "transfer.path", transferId, path });
      },
      onComplete: () => {},
      onCancelled: (transferId) => {
        closeDirect(transferId, "cancelled");
        applyTransferEvent({
          kind: "transfer.state",
          transferId,
          state: TRANSFER.CANCELLED,
          code: "CANCELLED",
        });
        view.announce("Trasferimento annullato");
        refreshResumable();
      },
      onStaged: (info) => {
        closeDirect(info.transferId, "staged");
        applyTransferEvent({
          kind: "transfer.state",
          transferId: info.transferId,
          state: TRANSFER.VERIFIED,
        });
        showStagedFile(info.transferId);
        view.announce("Download verificato, pronto da salvare");
        refreshResumable();
      },
      onAttemptFailed: (transferId, attemptId, reason) => {
        // The receive pipeline decided this ATTEMPT cannot be followed — a
        // carrier that stopped rather than fell behind, or a frame stream
        // that stopped meaning what the attempt assumes. It is the same
        // outcome as a channel that died, so it takes the same path: the
        // direct attempt ends, the verified ranges go to the server and the
        // transfer continues on the relay. The transfer itself is untouched.
        void failDirect(transferId, attemptId, reason, true);
      },
      onError: (transferId, code, detail) => {
        if (typeof transferId === "string") {
          closeDirect(transferId, "transfer-error");
        }
        try {
          const hook = globalThis.__BORE_TEST__;
          if (hook && Array.isArray(hook.receiverErrors)) {
            hook.receiverErrors.push(
              detail === undefined
                ? String(code)
                : `${code}: ${String(detail).slice(0, 160)}`,
            );
          }
        } catch {
          /* recording must never break dispatch */
        }
        if (typeof transferId === "string") {
          const row = state.transfers.get(transferId);
          if (code === "SOURCE_CHANGED" && row !== undefined) {
            // The partial is NOT discarded: it is what this peer verified,
            // and only the user decides it is worth less than a restart.
            applyTransferEvent({
              kind: "transfer.source_changed",
              offerId: row.offerId,
            });
          }
          applyTransferEvent({
            kind: "transfer.state",
            transferId,
            state: TRANSFER.FAILED,
            code,
          });
        }
        // Stable text per code, never the server's own words.
        view.announce(errorText(code));
        refreshResumable();
      },
    },
  });
}

/** Shows the staged file with explicit save/discard (never automatic). */
function showStagedFile(transferId) {
  const staged = receiver?.staged().get(transferId);
  if (!staged) {
    return;
  }
  const fileName = sanitizeDownloadName(staged.fileName);
  view.showVerifiedFile({
    fileName,
    onSave: async () => {
      // showSaveFilePicker is a runtime-detected enhancement and needs the
      // user activation this handler runs in; the anchor below is the
      // mandatory fallback.
      if (typeof window.showSaveFilePicker === "function") {
        try {
          const handle = await window.showSaveFilePicker({
            suggestedName: fileName,
          });
          const writable = await handle.createWritable();
          const fresh = receiver?.staged().get(transferId);
          if (!fresh) {
            try {
              await writable.close();
            } catch {
              /* best effort */
            }
            return;
          }
          // The staged Blob itself, never a fetch of its object URL: the
          // page's own CSP is `connect-src 'self'`, so fetching a `blob:`
          // URL is refused and the picker save would silently do nothing.
          const file = fresh.blob ?? null;
          if (file === null) {
            try {
              await writable.close();
            } catch {
              /* best effort */
            }
            return;
          }
          await writable.write(file);
          await writable.close();
          await receiver?.confirmSaved(transferId);
          view.hideVerifiedFile();
          applyTransferEvent({
            kind: "transfer.state",
            transferId,
            state: TRANSFER.DONE,
          });
          view.announce("File salvato");
          refreshResumable();
          return;
        } catch {
          // Denied or failed: fall through to the anchor download.
        }
      }
      // Anchor downloads fetch asynchronously after the click: revoking now
      // would cancel them, so the URL joins the spent list (revoked on room
      // close, page hide or the next save) while staging purges at once.
      const anchor = document.createElement("a");
      anchor.href = staged.url;
      anchor.download = fileName;
      anchor.rel = "noopener";
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      await receiver?.confirmSaved(transferId, {
        keepUrl: true,
        keepBytes: true,
      });
      spentAnchorUrls.push(staged.url);
      view.hideVerifiedFile();
      applyTransferEvent({
        kind: "transfer.state",
        transferId,
        state: TRANSFER.DONE,
      });
      view.announce("File salvato");
      refreshResumable();
    },
    onDiscard: async () => {
      await receiver?.discardStaged(transferId);
      view.hideVerifiedFile();
      // Discarding throws the bytes away: the row goes with them.
      applyTransferEvent({ kind: "transfer.removed", transferId });
      view.announce("Download scartato");
      refreshResumable();
    },
  });
}

/** Object URLs of finished anchor downloads (revoked on close/hide/save). */
const spentAnchorUrls = [];

function revokeSpentAnchorUrls() {
  while (spentAnchorUrls.length > 0) {
    const url = spentAnchorUrls.pop();
    try {
      URL.revokeObjectURL(url);
    } catch {
      /* best effort */
    }
  }
}

/** Source transfer actor (3.3): auto-ready, relay attach, send pipeline. */
function makeSender() {
  return createSender({
    offers,
    sendControl: (message) => {
      if (session === null) {
        return false;
      }
      return session.send(
        message.type,
        message.requestId ?? null,
        message.body,
      );
    },
    createSocket: (url, protocol) => new WebSocket(url, protocol),
    relayBase: relayBase(),
    roomIdHex: roomId,
    roomKeyHex: secrets.roomKey,
    getSelfPeerId: () => selfPeerId,
    events: {
      onStarted: (info) => {
        applyTransferEvent({
          kind: "transfer.started",
          transferId: info.transferId,
          offerId: info.offerId,
          direction: "out",
          sourcePeerId: selfPeerId,
          recipientPeerId: info.recipientPeerId,
          label: info.label,
          totalBytes: info.totalBytes,
        });
      },
      onProgress: (info) => {
        applyTransferEvent({
          kind: "transfer.progress",
          transferId: info.transferId,
          doneBytes: info.sentBytes,
          totalBytes: info.totalBytes,
          bytesPerSecond: trackSpeed(info.transferId, info.sentBytes),
        });
      },
      onPath: (transferId, path) => {
        // The SOURCE learns its path only from the recipient's forwarded
        // report: its own socket cannot tell a relay leg from a DataChannel,
        // and a path nobody has verified a byte over is not a fact yet.
        applyTransferEvent({ kind: "transfer.path", transferId, path });
      },
      onChunk: () => {},
      onEntryDone: () => {},
      onCancelled: (transferId) => {
        closeDirect(transferId, "cancelled");
        applyTransferEvent({
          kind: "transfer.state",
          transferId,
          state: TRANSFER.CANCELLED,
          code: "CANCELLED",
        });
        view.announce("Trasferimento annullato");
      },
      onDone: (transferId) => {
        closeDirect(transferId, "done");
        applyTransferEvent({
          kind: "transfer.state",
          transferId,
          state: TRANSFER.DONE,
        });
        view.announce("Trasferimento completato");
      },
      onError: (transferId, code) => {
        if (typeof transferId === "string") {
          closeDirect(transferId, "transfer-error");
        }
        try {
          const hook = globalThis.__BORE_TEST__;
          if (hook && Array.isArray(hook.senderErrors)) {
            hook.senderErrors.push(String(code));
          }
        } catch {
          /* recording must never break dispatch */
        }
        if (typeof transferId === "string") {
          applyTransferEvent({
            kind: "transfer.state",
            transferId,
            state: TRANSFER.FAILED,
            code,
          });
        }
        view.announce(errorText(code));
      },
      onSourceChanged: (offerId) => {
        offers?.withdrawOffer(offerId);
      },
    },
  });
}

// Network-change recovery (2.6): a dead link is otherwise silent until the
// next heartbeat or close frame — on mobile networks that strands the room
// for a full cycle. Cycling drops the socket so the backoff redials at once;
// the reconnect then re-hellos and the snapshot path republishes.
window.addEventListener("offline", () => session?.cycle());
window.addEventListener("online", () => session?.cycle());
// Spent anchor URLs die with the tab at the latest (their bytes live in
// OPFS, so holding them only pins registry entries, never content).
window.addEventListener("pagehide", () => revokeSpentAnchorUrls());

function startSession() {
  setConnection(CONNECTION.CONNECTING, "Connessione alla room…");
  offers = makeOffers();
  sender = makeSender();
  receiver = makeReceiver();
  session = createControlSession({
    url: controlUrl(),
    memberToken: secrets.memberToken,
    displayName: null,
    events: {
      onMessage: (message) => {
        const hook = globalThis.__BORE_TEST__;
        if (
          hook &&
          Array.isArray(hook.inboundTypes) &&
          typeof message?.type === "string"
        ) {
          try {
            hook.inboundTypes.push(message.type);
            if (Array.isArray(hook.inboundMarks)) {
              // The read counter AT the moment each control message landed.
              // A commit whose mark equals the incoming's mark proves no
              // payload byte was read while the path was being negotiated.
              hook.inboundMarks.push({
                type: message.type,
                reads: (hook.fileReads ?? []).length,
                // The CODE of a refusal, because "an error arrived" and "the
                // server refused this exact thing" are different findings and
                // a gate that cannot tell them apart costs a whole run.
                ...(message.type === "error"
                  ? { code: message.body?.code ?? null }
                  : {}),
              });
            }
            if (
              message.type === "transfer.relay_ticket" &&
              Array.isArray(hook.relayTickets)
            ) {
              hook.relayTickets.push(message.body);
            }
            if (
              message.type === "transfer.path_commit" &&
              Array.isArray(hook.pathCommits)
            ) {
              hook.pathCommits.push(message.body);
            }
            // 4.4: the SOURCE's only evidence about the path is this
            // notice — the server's word, derived from the recipient's
            // report. Recorded so a test can prove the badge followed it
            // instead of the source's own opinion.
            if (
              message.type === "transfer.progress" &&
              Array.isArray(hook.progressNotices)
            ) {
              hook.progressNotices.push(message.body);
            }
            if (message.type === "error" && Array.isArray(hook.controlErrors)) {
              hook.controlErrors.push({
                ...message.body,
                sent: hook.outboundById?.[message.body?.requestId] ?? null,
              });
            }
          } catch {
            /* recording must never break dispatch */
          }
        }
        if (
          message?.type === "welcome" &&
          typeof message?.body?.peerId === "string"
        ) {
          selfPeerId = message.body.peerId;
          try {
            hook.selfPeerId = selfPeerId;
          } catch {
            /* recording must never break dispatch */
          }
        }
        if (message?.type === "welcome" && message?.body?.limits) {
          serverLimits = message.body.limits;
          offers?.setServerLimits(serverLimits);
        }
        if (
          (message?.type === "ack" || message?.type === "error") &&
          offers?.handleReply(
            message.type,
            message.body,
            message.requestId ?? null,
          )
        ) {
          renderOffers();
          return;
        }
        // Direct negotiation (4.2). Everything WebRTC lives behind these
        // four message types and `webrtc.js`; the transfer actors below see
        // only a transport that is ready or gone.
        if (message?.type === "transfer.direct_start") {
          startDirectAttempt(message.body ?? {});
          return;
        }
        if (
          message?.type === "rtc.offer" ||
          message?.type === "rtc.answer" ||
          message?.type === "rtc.ice"
        ) {
          routeSignal(message.type, message.body ?? {});
          return;
        }
        if (message?.type === "transfer.direct_failed") {
          // The COUNTERPART gave up. Our side drops the attempt without
          // sending a notice of its own: the server already knows, and the
          // relay attempt it minted arrives as a ticket.
          void abandonDirect(message.body ?? {});
          return;
        }
        // Source-side transfer traffic (incoming/ticket/commit/cancel):
        // the sender consumes what belongs to a tracked transfer.
        if (sender?.handleControl(message)) {
          return;
        }
        // Recipient-side traffic (request acks, tickets, commits, staged
        // completion): the receiver consumes its own transfers.
        if (receiver?.handleControl(message)) {
          return;
        }
        const event = messageToEvent(message);
        if (event === null) {
          return;
        }
        state = reduce(state, event);
        if (event.kind === "room.closed") {
          // Authoritative purge: staging, URLs, records and live legs go.
          closeAllDirect();
          view.hideVerifiedFile();
          receiver?.purgeAll().catch(() => {});
          teardownUnavailable("Room non disponibile");
          return;
        }
        if (event.kind === "snapshot.end" && offers !== null) {
          // Republish after reconnect: absent IDs go now, IDs still held
          // by our ghost session wait for their `offer.removed` (2.6).
          const live = new Set(state.offers.keys());
          const { now, later } = partitionRepublish(
            offers.republishCandidates(),
            live,
          );
          for (const id of now) {
            offers.republish(id);
          }
          for (const id of later) {
            deferredRepublish.add(id);
          }
          // A reconnect re-reads what is on disk and marks those offers
          // resumable. It sends no request: only a click does that.
          refreshResumable();
        }
        // The manifest travelled through the server, which cannot forge this
        // tag: an offer that does not authenticate under the room key never
        // reaches the catalog, so no filename and no size the room did not
        // write is ever shown. Both entry points are covered — a peer that
        // joins later receives the same offers as `snapshot.offer`. The
        // receiver checks again before it requests anything (defence in
        // depth, not a duplicate).
        if (
          (event.kind === "offer.added" || event.kind === "snapshot.offer") &&
          event.peerId !== selfPeerId
        ) {
          dropUnauthenticOffer(event);
        }
        if (event.kind === "offer.added" && event.peerId !== selfPeerId) {
          // Availability is announced, never acted on: no filename leaves
          // the local UI through this line.
          view.announce("Nuova offerta disponibile");
        }
        if (event.kind === "offer.removed") {
          // Withdraw purges this offer's staging first, then the republish
          // machinery decides. The partial is gone, so the restart offer
          // that belonged to it goes with it.
          applyTransferEvent({
            kind: "transfer.source_changed",
            offerId: event.offerId,
            changed: false,
          });
          receiver
            ?.purgeOffer(event.offerId)
            .then(refreshResumable)
            .catch(() => {});
          if (deferredRepublish.has(event.offerId)) {
            deferredRepublish.delete(event.offerId);
            offers?.republish(event.offerId);
          }
        }
        render();
      },
      onHelloAck: () => {
        setConnection(CONNECTION.CONNECTED, "Connesso alla room");
      },
      onClose: (code, terminal) => {
        if (terminal) {
          teardownUnavailable("Room non disponibile");
          return;
        }
        setConnection(CONNECTION.RECONNECTING, "Riconnessione…");
      },
      onStateChange: (next) => {
        if (next === "reconnecting") {
          setConnection(CONNECTION.RECONNECTING, "Riconnessione…");
        }
      },
    },
  });
  session.start();
}

// Test-only introspection (same `__BORE_TEST__` pattern as the transfer
// note): exposes the live catalog for the e2e MAC cross-check. Absent
// without the hook; never used by production code paths.
if (
  typeof globalThis.__BORE_TEST__ === "object" &&
  globalThis.__BORE_TEST__ !== null
) {
  const hook = globalThis.__BORE_TEST__;
  hook.getCatalogSnapshot = () =>
    [...state.offers].map(([offerId, offer]) => ({
      offerId,
      peerId: offer.peerId,
      manifest: offer.manifest,
      mac: offer.mac,
    }));
  // 2.6 recorders: outbound control types, constructed socket URLs,
  // main-thread slice reads, RTC constructions, live transfer rows. Arrays
  // only — the app never reads them back.
  if (!Array.isArray(hook.outboundTypes)) {
    hook.outboundTypes = [];
  }
  // 3.5: resume descriptors per `transfer.request` (null for a fresh one).
  if (!Array.isArray(hook.resumeRequests)) {
    hook.resumeRequests = [];
  }
  // 3.3 recorders: inbound control types (path_commit/ticket timing without
  // reading app state), a raw control sender (the e2e recipient has no
  // download UI yet), the live sender table, and an offer-file swapper for
  // the source-change path. Arrays only — the app never reads them back.
  if (!Array.isArray(hook.inboundTypes)) {
    hook.inboundTypes = [];
  }
  if (typeof hook.controlSend !== "function") {
    hook.controlSend = (type, requestId, body) =>
      session?.send(type, requestId, body) ?? false;
  }
  if (typeof hook.senderState !== "function") {
    hook.senderState = () =>
      sender === null
        ? []
        : [...sender.transfers().values()].map((transfer) => ({
            transferId: transfer.transferId,
            attemptId: transfer.attemptId,
            state: transfer.state,
            sentBytes: transfer.sentBytes,
          }));
  }
  // 3.3 recipient-side taps (the e2e recipient has no download UI yet, so it
  // drives the relay by hand): relay tickets, control errors and own peer ID.
  if (!Array.isArray(hook.relayTickets)) {
    hook.relayTickets = [];
  }
  if (!Array.isArray(hook.controlErrors)) {
    hook.controlErrors = [];
  }
  if (hook.outboundById === undefined) {
    hook.outboundById = {};
  }
  // 4.2 recorders: the direct attempt's lifecycle and the committed path.
  // Neither ever carries SDP, a candidate or a key.
  if (!Array.isArray(hook.directEvents)) {
    hook.directEvents = [];
  }
  if (!Array.isArray(hook.pathCommits)) {
    hook.pathCommits = [];
  }
  if (!Array.isArray(hook.progressNotices)) {
    hook.progressNotices = [];
  }
  if (!Array.isArray(hook.inboundMarks)) {
    hook.inboundMarks = [];
  }
  if (typeof hook.directState !== "function") {
    hook.directState = () =>
      [...directAttempts.entries()].map(([transferId, entry]) => ({
        transferId,
        attemptId: entry.attemptId,
        recipient: entry.recipient,
        ...entry.actor.state(),
      }));
  }
  if (!Array.isArray(hook.receiverErrors)) {
    hook.receiverErrors = [];
  }
  if (!Array.isArray(hook.senderErrors)) {
    hook.senderErrors = [];
  }
  if (typeof hook.replaceOfferFile !== "function") {
    hook.replaceOfferFile = (offerId, bytes, name, lastModified, path = null) => {
      const record = offers?.offers().get(offerId);
      if (!record || record.files.size === 0) {
        return false;
      }
      // Naming the path is what a FOLDER offer needs: changing one file of
      // a tree is the source change an archive can be asked to survive, and
      // the first file in insertion order is rarely the interesting one.
      const target = path ?? [...record.files.keys()][0];
      if (!record.files.has(target)) {
        return false;
      }
      const file = new File([bytes], name, {
        lastModified: lastModified ?? Date.now(),
      });
      record.files.set(target, { file });
      return true;
    };
  }
  // 3.4 explicit-download entry (3.5 buttons call the same path): resolves
  // the offer from the peer catalog and starts the receiver.
  if (typeof hook.requestDownload !== "function") {
    // Exactly what the button does — the hook must not become a second
    // path that could drift from the one users take.
    hook.requestDownload = (offerId, mode = "raw", options = {}) =>
      startDownload(offerId, mode, options);
  }
  if (typeof hook.receiverState !== "function") {
    hook.receiverState = () =>
      receiver === null
        ? []
        : [...receiver.transfers().values()].map((transfer) => ({
            transferId: transfer.transferId,
            state: transfer.state,
            receivedBytes: transfer.receivedBytes,
            // Who this transfer is WITH, and about what. The acceptance
            // scenario asserts that a row names the peer that published the
            // offer, and the attribution has to come from the transfer
            // itself rather than from a label rendered beside it.
            offerId: transfer.offerId,
            sourcePeerId: transfer.sourcePeerId,
            // How many chunk ranges are VERIFIED and on disk. Bytes off the
            // socket are not the same thing — hashing runs behind the wire —
            // and a gate about resuming verified work has to be able to tell
            // the two apart.
            verifiedRanges: transfer.verifiedRanges.length,
          }));
  }
  if (!Array.isArray(hook.wsUrls)) {
    hook.wsUrls = [];
  }
  if (!Array.isArray(hook.fileReads)) {
    hook.fileReads = [];
  }
  if (typeof hook.rtcConstructed !== "number") {
    hook.rtcConstructed = 0;
  }
  if (typeof hook.transferRows !== "function") {
    hook.transferRows = () => document.querySelectorAll(".transfer-row").length;
  }
  const RealWebSocket = window.WebSocket;
  window.WebSocket = function (url, protocols) {
    hook.wsUrls.push(String(url));
    return new RealWebSocket(url, protocols);
  };
  if (window.RTCPeerConnection) {
    const RealRTC = window.RTCPeerConnection;
    window.RTCPeerConnection = function (...args) {
      hook.rtcConstructed += 1;
      return new RealRTC(...args);
    };
  }
  const origSlice = Blob.prototype.slice;
  Blob.prototype.slice = function (...args) {
    hook.fileReads.push({ size: this.size });
    return origSlice.apply(this, args);
  };
}

const UNSUPPORTED_BROWSER =
  "Browser non supportato: WebCrypto HKDF non disponibile";

/// Boot order is deliberate: parse the persistent fragment, await all three
/// independent HKDF derivations, publish credentials only in module memory,
/// then start the control session. A malformed link never opens a socket.
async function boot() {
  installDiagnosticsHook();

  let parsed;
  try {
    parsed = parseShortRoomUrl(window.location.href);
  } catch {
    setConnection(CONNECTION.INCOMPLETE, "Link incompleto");
    render();
    return;
  }

  let material;
  try {
    material = await deriveRoomLinkMaterial(parsed.seed);
  } catch {
    setConnection(CONNECTION.UNAVAILABLE, UNSUPPORTED_BROWSER);
    render();
    return;
  }

  roomSeed = parsed.seed;
  roomId = material.roomId;
  secrets = {
    memberToken: material.memberToken,
    roomKey: material.roomKey,
  };
  startSession();
  render();
}

boot().catch(() => {
  roomSeed = null;
  roomId = null;
  secrets = null;
  setConnection(CONNECTION.UNAVAILABLE, UNSUPPORTED_BROWSER);
  render();
});
