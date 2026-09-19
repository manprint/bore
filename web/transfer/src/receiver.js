// Recipient transfer actor (Phase 3.4, direct-first since 4.2):
// quota-checked explicit downloads into OPFS with per-chunk verification,
// crash-safe resume and an explicit save step. Frames arrive either off a
// WebRTC DataChannel (`deliverDirectFrame`) or off the relay leg; nothing
// below the transport knows which, and an attempt that ends hands the next
// one the chunks already on disk. The server stays opaque: it sees ciphertext frames and
// control IDs, never keys, plaintext or filenames.
//
// Write discipline: a chunk lands on disk only after its digest matches the
// manifest, and the IDB record commits only after the OPFS write closes. A
// cancel keeps the partial for resume; withdraw and room close purge it.
// Completion needs the manifest length AND entry root before
// `transfer.complete` leaves the tab.

import {
  attemptKey,
  bytesToHex,
  fileRoot,
  hexToBytes,
  importAttemptAesKey,
  manifestMac,
  sha256Hex,
} from "./crypto.js";
import { perfEnd, perfPeak, perfStart } from "./perf.js";
import { canonicalize, manifestValue } from "./protocol.js";
import {
  ARCHIVE_ENTRY_ID,
  CHUNK_BYTES,
  FRAME_DATA,
  FRAME_FINAL,
  peekFrameType,
  peekFrameSeq,
  chunkWindow,
  decodeFrameWithKey,
  parseArchiveFinalPayload,
  planSend,
} from "./framing.js";
import { archiveName } from "./zip-stream.js";
import {
  capRanges,
  chunkPartName,
  coalesceRanges,
  partDirSegments,
  partialKey,
  verifiedPrefix,
} from "./storage.js";

/**
 * Frames opened at once. Eight is where the in-page probe stops improving
 * (webkit 2150 MiB/s at four, 2481 at eight) and it bounds the plaintext in
 * flight to eight fragments, 192 KiB.
 */
export const FRAME_PIPELINE_DEPTH = 8;

/**
 * Frames held out of order while their predecessors are still in flight, and
 * the bytes they may occupy — PER CARRIER, which is the correction B-A040
 * made and the whole of it.
 *
 * One transport delivers a frame stream in order and never holds anything
 * here, so this costs nothing until a transfer runs on SEVERAL carriers, when
 * the arrival order is the order N independent associations happened to
 * deliver in. The window absorbs the skew between them; it does not absorb a
 * carrier that has stopped.
 *
 * The ceiling used to be a FIXED 8 MiB, sized — the old comment said so in as
 * many words — as "about a second of skew at the rate a single association
 * sustains". That is the wrong rate. While one carrier is paused the window
 * fills at the rate of the OTHER N-1 combined, so the time it buys falls as
 * the carrier count rises: at 8 carriers and ~5.5 MB/s per association, one
 * ordinary SCTP retransmission on one carrier fills 8 MiB in about 200 ms.
 * MEASURED between two hosts 21 ms apart, 128 MiB, wired: at 8 carriers the
 * window peaked at 8 407 808 bytes — its ceiling — in 344 frames after 1 270
 * holds, and the recipient abandoned a direct path that was delivering, 1.3 s
 * after the channels opened. 3/3 transfers fell back at 8 carriers, 1/3 at
 * the shipped default of 4, never at 1 or 2. Scaling the budget with the
 * carrier count is what makes the window mean the same thing at every count.
 *
 * The absolute ceiling exists because this lives in the tab's own heap and a
 * peer chooses the carrier count: the budget grows with carriers, never past
 * this.
 *
 * Overflowing is a failure of the ATTEMPT, not of the transfer: the direct
 * path is abandoned, the transfer continues on the relay from the ranges
 * already verified on disk, and the user sees a slower transfer rather than a
 * failed one.
 */
export const REORDER_MAX_FRAMES_PER_CARRIER = 512;
/** … and the byte ceiling, reached first whenever fragments are full size. */
export const REORDER_MAX_BYTES_PER_CARRIER = 8 * 1024 * 1024;
/** Hard caps on the scaled budget, whatever carrier count a peer announces. */
export const REORDER_ABS_MAX_FRAMES = 4096;
export const REORDER_ABS_MAX_BYTES = 64 * 1024 * 1024;
/**
 * How long the window may hold a gap that is not filling before the attempt
 * is abandoned.
 *
 * This is the test the byte ceiling was standing in for, and it is the one
 * that actually distinguishes the two cases the old comment named: a carrier
 * that is BEHIND keeps advancing `reorderNext` and a carrier that has STOPPED
 * does not. A byte ceiling measures neither — it measures how fast the other
 * carriers are, so on a fast path it fires on healthy skew and on a slow one
 * it lets a genuinely dead carrier hold the transfer for minutes. Both
 * remain: the deadline is the verdict, the budget is the memory bound.
 */
export const REORDER_STALL_MS = 4_000;

/**
 * The window budget for a transfer announced with `carriers` carriers.
 * Pure so the sizing can be tested without a transport.
 * @param {number} carriers
 * @returns {{frames: number, bytes: number}}
 */
export function reorderBudget(carriers) {
  const n = Math.max(1, Math.min(Number(carriers) || 1, 64));
  return {
    frames: Math.min(REORDER_ABS_MAX_FRAMES, REORDER_MAX_FRAMES_PER_CARRIER * n),
    bytes: Math.min(REORDER_ABS_MAX_BYTES, REORDER_MAX_BYTES_PER_CARRIER * n),
  };
}

/**
 * How long a DIRECT attempt may deliver NOTHING, while the recipient has
 * nothing of its own left to do, before the attempt is abandoned.
 *
 * A carrier that closes reports itself and the group notices immediately.
 * A carrier that goes SILENT does not: a NAT that forgets the UDP flow, a
 * radio that drops, a peer whose tab is frozen — DTLS holds the channel
 * `open` and every frame the source writes disappears. Nothing in the path
 * had a deadline, so the download sat at its last verified byte for ever and
 * only a reload could end it. The server is not on the direct path and cannot
 * see this; the recipient is the only party that can.
 *
 * The clock runs ONLY while the recipient is idle — nothing in the inbox and
 * no pump running. That distinction is what keeps a SLOW recipient alive: a
 * recipient that is behind makes the source's queue fill, so frames stop
 * arriving for a reason that has nothing to do with the path, and a timer
 * that did not look would abandon a perfectly healthy attempt under exactly
 * the load the carriers exist to serve.
 *
 * 20 s is long against every legitimate pause with an idle recipient (the
 * source reading its next chunk off disk) and short against a user watching a
 * row that will never move again. The attempt dies, never the transfer: the
 * relay finishes it from the ranges already verified on disk.
 */
export const DIRECT_IDLE_TIMEOUT_MS = 20_000;
/** How often the idle clock is read. Coarse on purpose: this must cost
 * nothing on a transfer that is running. */
export const DIRECT_IDLE_CHECK_MS = 1_000;

/**
 * A verified-bytes report leaves at most this often, PER LIVE TRANSFER SHARE.
 *
 * `transfer.progress` is a control message and spends from the session's
 * mutation bucket, which the server sizes for room mutations: 4 a second,
 * burst 8. This used to report every `PROGRESS_MIN_MS` **or** every megabyte
 * of newly verified bytes, whichever came first — and on any path worth
 * having, the megabyte rule is the one that fires. MEASURED on loopback with
 * a 256 MiB transfer: ~50 reports a second, the bucket empty within the first
 * second, and then **`transfer.complete` itself refused `RATE_LIMITED`** —
 * the one terminal message of the transfer, dropped, with every byte received
 * and verified on disk. The row sat at `complete-pending` for as long as it
 * was watched (236 s), the file was never offered for saving, and the source
 * never learned the transfer had finished. The faster the path, the more
 * certain the failure, which is exactly backwards.
 *
 * A progress report is OBSERVABILITY, and observability must never cost the
 * data path. So the cadence is a RATE and nothing else, and the rate is
 * divided among this peer's live transfers: eight concurrent downloads report
 * once every four seconds each, which is the same two a second in total and
 * leaves the rest of the bucket for the messages that carry meaning.
 */
export const PROGRESS_MIN_MS = 500;

/**
 * How many times `transfer.complete` is re-sent after a RETRYABLE refusal.
 *
 * The cadence fix above removes the cause, but not the exposure: the mutation
 * bucket is shared with every other control message this peer sends, so a
 * burst — several transfers finishing together, a rename, a republish — can
 * still refuse the one message that ends a transfer. Every byte is on disk
 * and verified at that point, so a dropped `transfer.complete` is a transfer
 * that can never finish and a file the user can never save.
 *
 * It is therefore re-sent, on a growing grid, with a fresh `requestId` each
 * time (the server answers per request, and a reused ID would be answered
 * from the first verdict). Completion is idempotent server-side, so a retry
 * that crosses a late ack costs an ignored answer and nothing more. When the
 * grid runs out the transfer FAILS LOUDLY with the refusal's own code —
 * a visible failure the user can act on, never a row frozen at 100%.
 */
export const COMPLETE_MAX_RETRIES = 8;

/**
 * Whether a verified-bytes report may leave now.
 *
 * Pure, and exported, because it is the whole policy and the policy is what
 * broke: the rule used to be "every `PROGRESS_MIN_MS` **or** every megabyte,
 * whichever comes first", and on a fast path the megabyte is always first.
 * There is no byte term here at all, by design — bytes are what the report is
 * ABOUT, never what decides that it is sent.
 *
 * @param {number} now `Date.now()`
 * @param {number} lastReportAt when this transfer last reported
 * @param {boolean} first no report has left for this transfer yet
 * @param {number} liveTransfers how many transfers this peer has in flight
 */
export function progressIsDue(now, lastReportAt, first, liveTransfers) {
  if (first) {
    // One per transfer cannot flood anything, and it is what moves the bar
    // off zero and resolves the path badge.
    return true;
  }
  return now - lastReportAt >= PROGRESS_MIN_MS * Math.max(1, liveTransfers);
}
/** First backoff before re-sending `transfer.complete`; doubles each time. */
export const COMPLETE_RETRY_BASE_MS = 250;

function randomRequestId() {
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}

/**
 * @param {object} options
 * @param {(message: object) => boolean} options.sendControl control sender
 * @param {(url: string, protocol: string) => WebSocket} options.createSocket
 * relay socket factory (injected for tests)
 * @param {string} options.relayBase `ws(s)://host` prefix for relay URLs
 * @param {string} options.roomIdHex canonical room ID
 * @param {string} options.roomKeyHex 64-hex room key (memory only)
 * @param {() => string|null} options.getSelfPeerId own peer ID
 * @param {object} options.repository OPFS/IDB repository (see storage.js)
 * @param {object} options.events `{ onStarted(info), onProgress(info),
 * onComplete(info), onStaged(info), onCancelled(transferId, offerId),
 * onError(transferId, code) }` (all optional)
 */
export function createReceiver({
  sendControl,
  createSocket,
  relayBase,
  roomIdHex,
  roomKeyHex,
  getSelfPeerId,
  repository,
  events = {},
  directIdleTimeoutMs = DIRECT_IDLE_TIMEOUT_MS,
  directIdleCheckMs = DIRECT_IDLE_CHECK_MS,
  reorderStallMs = REORDER_STALL_MS,
}) {
  /** transferId → live download state (deleted at every terminal step). */
  const transfers = new Map();
  /** requestId → pending request (bound to a transfer on ack). */
  const pendingRequests = new Map();
  /**
   * complete requestId → `{ transferId, body, tries }`, the recipient's only
   * completion signal. It holds the BODY because a refusal has to be
   * answerable by sending the same statement again.
   */
  const pendingCompletes = new Map();
  /** Staged verified files awaiting explicit save/discard. */
  const stagedFiles = new Map();

  function forget(transferId) {
    const transfer = transfers.get(transferId);
    if (transfer === undefined) {
      return;
    }
    transfer.abort.abort();
    stopIdleClock(transfer);
    try {
      transfer.socket?.close();
    } catch {
      /* already gone */
    }
    transfer.aesKey = null;
    transfers.delete(transferId);
    for (const [requestId, bound] of pendingCompletes) {
      if (bound.transferId === transferId) {
        pendingCompletes.delete(requestId);
      }
    }
  }

  /** Stops the direct idle clock, wherever the attempt ended. */
  function stopIdleClock(transfer) {
    if (transfer.idleTimer !== null && transfer.idleTimer !== undefined) {
      clearInterval(transfer.idleTimer);
      transfer.idleTimer = null;
    }
  }

  /**
   * Arms the idle clock for the direct attempt now current. Re-armable: the
   * previous attempt's clock is stopped first, so an upgrade or a fallback
   * never leaves two running.
   */
  /**
   * Names the transport of the CURRENT attempt, once, and corrects it if the
   * commit that decides it lost a race to the first verified chunk.
   *
   * The badge's rule is that only a verified chunk may name a transport, and
   * that rule stands. But the transport a chunk proves is read off
   * `transfer.transport`, which starts at `"relay"` and is set by
   * `transfer.path_commit` — a CONTROL message, on a different socket from
   * the frames. With several carriers the first frame regularly beats it:
   * MEASURED on loopback with four carriers, the recipient verified chunk 0
   * before dispatching the commit, latched `relay` for the attempt and then
   * never told the truth again, on a transfer that ran entirely direct and
   * whose bytes were correct. So the commit re-names the path when what it
   * says disagrees with what was latched FOR THE SAME ATTEMPT; a different
   * attempt is a different badge and is left alone.
   *
   * @param {object} transfer live download state
   */
  function nameTransport(transfer) {
    if (
      transfer.pathAttempt === transfer.attemptId &&
      transfer.path === transfer.transport
    ) {
      return;
    }
    transfer.pathAttempt = transfer.attemptId;
    transfer.path = transfer.transport;
    events.onPath?.(transfer.transferId, transfer.transport);
  }

  function startIdleClock(transfer) {
    stopIdleClock(transfer);
    if (!(directIdleTimeoutMs > 0)) {
      return;
    }
    transfer.lastFrameAt = Date.now();
    transfer.idleTimer = setInterval(() => {
      const live = transfers.get(transfer.transferId);
      if (live !== transfer || transfer.attemptClosed) {
        stopIdleClock(transfer);
        return;
      }
      if (transfer.state !== "direct" && transfer.state !== "receiving") {
        // Negotiating, relaying, or past the last byte: not this clock's
        // business. `complete-pending` in particular is a recipient that is
        // idle ON PURPOSE, waiting for the server's ack.
        stopIdleClock(transfer);
        return;
      }
      if (transfer.pumping || transfer.inbox.length > 0) {
        // Busy with what already arrived: the path owes us nothing yet.
        transfer.lastFrameAt = Date.now();
        return;
      }
      if (Date.now() - transfer.lastFrameAt < directIdleTimeoutMs) {
        return;
      }
      stopIdleClock(transfer);
      failAttempt(transfer, "stalled");
    }, directIdleCheckMs);
    // Keeps its `unref` (B-A034): this is a repeating poller, and the timer
    // itself is what a waiting test sees as a live handle — reffing it would
    // instead hold a Node process open for the whole life of the transfer.
    if (typeof transfer.idleTimer?.unref === "function") {
      transfer.idleTimer.unref();
    }
  }

  /** Local abort that keeps the verified partial for resume. */
  function abortTransfer(transferId) {
    if (!transfers.has(transferId)) {
      return false;
    }
    forget(transferId);
    return true;
  }

  /**
   * Rebuilds the per-attempt receive state for `attemptId`: a new key, a new
   * nonce sequence from zero, and a send plan derived from what is VERIFIED
   * on disk right now. Bytes of a half-received chunk are dropped — only a
   * whole chunk that matched its manifest digest is on disk, so nothing that
   * survives here is unverified.
   */
  function adoptAttempt(transfer, attemptId) {
    transfer.attemptId = attemptId;
    stopIdleClock(transfer);
    transfer.aesKey = null;
    transfer.pendingFrames = [];
    transfer.inbox = [];
    transfer.reorder = new Map();
    transfer.reorderBytes = 0;
    transfer.reorderGapSince = 0;
    transfer.reorderNext = 0;
    transfer.reorderWindow = false;
    // A pump still draining the DEAD attempt's last batch returns without
    // touching this flag (its attempt no longer matches), so clearing it
    // here is what lets the new attempt's first frame start a pump.
    transfer.pumping = false;
    transfer.attemptClosed = false;
    transfer.chunkBuffers = [];
    transfer.expectedSeq = 0;
    transfer.receivedBytes = 0;
    transfer.lastReportAt = 0;
    transfer.lastReportBytes = null;
    // The new attempt has carried nothing yet, so its transport is not a fact
    // yet either — but the path the row already EARNED stays until the first
    // chunk of the new attempt replaces it, so the badge never flickers back
    // to `connecting` mid-transfer.
    if (transfer.archive) {
      // A NEW attempt starts from what this peer holds; the commit that
      // follows re-bases it on what the source was actually told to skip.
      replanArchive(transfer, transfer.verifiedRanges);
      return;
    }
    const kept = capRanges(coalesceRanges(transfer.verifiedRanges));
    transfer.verifiedRanges = kept;
    const { send } = planSend(transfer.chunkCount, kept);
    transfer.plan = send;
    transfer.planPos = 0;
    let expected = 0;
    for (const index of send) {
      expected += chunkWindow(Number(transfer.entry.size), index).length;
    }
    transfer.expectedBytes = expected;
  }

  /**
   * Re-bases an ARCHIVE attempt on the chunks the SOURCE was told to skip.
   *
   * The source regenerates the archive from byte zero — `ZIP_WRITER_OPTIONS`
   * fixes every field a ZIP writer would otherwise vary, so a second pass
   * over the same manifest and the same files is byte-identical — and sends
   * only the chunks outside `ranges`. An archive resumes on a PREFIX alone
   * (see `verifiedPrefix`), so the first chunk to arrive is chunk `n` and
   * this end must be sitting at exactly `n`: holding MORE than the source
   * was told to skip would stage the next arrival one index too high and
   * corrupt the rolling root, which is why this takes the ranges from the
   * message BOTH peers saw rather than from local disk (4.3's rule).
   *
   * `leaves` is in index order by construction, so truncating it is how the
   * root follows the re-plan. Nothing is deleted from disk: a chunk sent
   * again is simply staged and recorded again.
   */
  function replanArchive(transfer, ranges) {
    const have =
      transfer.verifiedRanges.length === 0 ? 0 : transfer.verifiedRanges[0][1];
    const prefix = verifiedPrefix(
      (Array.isArray(ranges) ? ranges : []).filter(
        (range) =>
          Array.isArray(range) &&
          range.length === 2 &&
          Number.isSafeInteger(range[0]) &&
          Number.isSafeInteger(range[1]) &&
          range[0] >= 0 &&
          range[1] >= range[0],
      ),
    );
    const asked = prefix.length === 0 ? 0 : prefix[0][1];
    // Never claim more than this peer actually verified: the source skips
    // what the SERVER recorded, and the server's copy can only be older.
    const keep = Math.min(asked, have);
    transfer.leaves.length = keep;
    transfer.archiveChunks = keep;
    transfer.archiveBytes = keep * CHUNK_BYTES;
    transfer.verifiedRanges = keep === 0 ? [] : [[0, keep]];
    transfer.chunkBuffers = [];
    transfer.plan = [];
    transfer.planPos = 0;
    transfer.expectedBytes = 0;
  }

  /**
   * Re-plans this attempt's receive order from the ranges the SERVER put on
   * `transfer.path_commit`. That message is the one both peers receive, so
   * taking the plan from it is what makes the two ends agree about which
   * chunk is on the wire. Planning from local disk instead cannot: after a
   * mid-transfer direct failure the recipient may hold MORE than the source
   * was told to skip (its failure report can lose the race to the source's
   * own), and the first relayed chunk then lands at the wrong plan position
   * and reads as a digest mismatch. What is already on disk is not lost —
   * `verifiedRanges` is untouched, so a chunk sent again is simply verified
   * and written again.
   *
   * @param {object} transfer live download state
   * @param {unknown} ranges the commit's `resumeRanges`, absent when none
   */
  function applyCommitPlan(transfer, ranges) {
    if (transfer.archive) {
      replanArchive(transfer, ranges);
      return;
    }
    const bounded = (Array.isArray(ranges) ? ranges : []).filter(
      (range) =>
        Array.isArray(range) &&
        range.length === 2 &&
        Number.isSafeInteger(range[0]) &&
        Number.isSafeInteger(range[1]) &&
        range[0] >= 0 &&
        range[1] >= range[0] &&
        range[1] <= transfer.chunkCount,
    );
    const { send } = planSend(
      transfer.chunkCount,
      capRanges(coalesceRanges(bounded)),
    );
    transfer.plan = send;
    transfer.planPos = 0;
    transfer.chunkBuffers = [];
    let expected = 0;
    for (const index of send) {
      expected += chunkWindow(Number(transfer.entry.size), index).length;
    }
    transfer.expectedBytes = expected;
  }

  /**
   * Ends one download on a failure THIS peer detected.
   *
   * The server is told, because a recipient that goes quiet leaves a live
   * record behind: the source has usually finished sending and sits waiting
   * for a completion that will never come, the relay permit and the
   * per-peer budget stay spent, and — the symptom that found this — the
   * next `transfer.request` for the same selection matches that live record
   * and is answered with the SAME transfer ID (`RequestOutcome::Existing`),
   * so the user clicks and nothing happens. `transfer.cancel` is the
   * message that says "not this one", and it is idempotent.
   *
   * `notifyServer` is false only when the SERVER is the one that ended the
   * transfer: it has already terminated the record and a cancel would ask
   * it about a transfer it no longer has.
   */
  function failTransfer(transferId, code, detail, notifyServer = true) {
    const transfer = transfers.get(transferId);
    if (transfer === undefined) {
      return;
    }
    events.onError?.(transferId, code, detail);
    forget(transferId);
    if (notifyServer) {
      sendControl({
        v: 1,
        type: "transfer.cancel",
        requestId: randomRequestId(),
        body: { transferId },
      });
    }
  }

  /**
   * True when `macHex` is the room's own tag over this manifest. Constant
   * work either way: the comparison is over hex we computed ourselves.
   */
  async function manifestMacMatches(manifest, macHex) {
    if (typeof macHex !== "string" || !/^[0-9a-f]{64}$/.test(macHex)) {
      return false;
    }
    try {
      const canonical = new TextEncoder().encode(
        canonicalize(manifestValue(manifest)),
      );
      const expected = bytesToHex(
        await manifestMac(
          hexToBytes(roomKeyHex),
          hexToBytes(roomIdHex),
          canonical,
        ),
      );
      return expected === macHex;
    } catch {
      return false;
    }
  }

  /**
   * Starts one explicit download. Only the click path (3.5 buttons, 3.4
   * test hook) calls this — join, reconnect and heartbeat never do.
   *
   * `mode` is `raw` (one manifest entry, verified chunk by chunk against the
   * manifest's own digests) or `zip` (the whole offer as one generated
   * archive, whose length, chunk count and root are not in the manifest and
   * arrive authenticated in the FINAL frame).
   *
   * `entryId` picks WHICH entry a `raw` download asks for. Absent, a
   * single-file offer serves its only entry; naming one is how a file
   * inside a folder is downloaded on its own, which the server has always
   * allowed (`validate_selection`: raw is exactly one FILE entry of this
   * manifest, whichever one).
   *
   * `fresh` throws away the partial for this exact selection before
   * planning. It is the ONLY way a verified partial is discarded without
   * the offer being withdrawn or the room closing, and nothing reaches it
   * except an explicit second gesture by the user after a `SOURCE_CHANGED`.
   */
  async function startDownload({
    offerId,
    manifest,
    macHex,
    sourcePeerId,
    mode = "raw",
    entryId = null,
    fresh = false,
  }) {
    const setupAt = perfStart();
    // The manifest arrives THROUGH the server, which cannot compute this
    // tag: only a peer holding the room key can. Checking it here is what
    // stops a forged manifest from redirecting a download onto
    // attacker-chosen roots — every later per-chunk digest would then verify
    // against the forgery and report success.
    if ((await manifestMacMatches(manifest, macHex)) !== true) {
      return { error: "MANIFEST_MAC" };
    }
    if ((await repository.supported()) !== true) {
      return { error: "UNSUPPORTED" };
    }
    const archive = mode === "zip";
    const allEntries = Array.isArray(manifest?.entries) ? manifest.entries : [];
    const fileEntries = allEntries.filter(
      (entry) => Array.isArray(entry.chunks) && entry.chunks.length > 0,
    );
    if (archive ? allEntries.length === 0 : fileEntries.length === 0) {
      return { error: "OFFER_NOT_FOUND" };
    }
    // A folder or multi-file offer is announced and signed exactly like a
    // single file; only the RAW download side is single-entry. Say that,
    // instead of reporting an offer that is plainly on screen as missing.
    // Naming an entry is how one file of a folder is downloaded; without a
    // name a multi-entry offer has no single raw answer, and saying so is
    // better than reporting an offer that is plainly on screen as missing.
    const chosen =
      entryId === null || entryId === undefined
        ? null
        : fileEntries.find((each) => String(each.id) === String(entryId));
    if (!archive && chosen === undefined) {
      return { error: "OFFER_NOT_FOUND" };
    }
    if (!archive && chosen === null && fileEntries.length !== 1) {
      return { error: "MULTI_ENTRY" };
    }
    // The archive is GENERATED, so its size, chunk count and root cannot be
    // known before it exists: the synthetic entry carries the name and the
    // reserved ID, and the rest arrives in the sealed FINAL frame. The ID is
    // one the server refuses inside a manifest (`RESERVED_ZIP_ENTRY_ID`),
    // which is what keeps an archive's staged parts, resume key and progress
    // reports from ever colliding with a file's.
    const entry = archive
      ? {
          id: String(ARCHIVE_ENTRY_ID),
          path: archiveName(manifest?.label),
          size: 0,
          chunkCount: 0,
          chunks: [],
          root: null,
        }
      : (chosen ?? fileEntries[0]);
    // The server rejects an unsorted `entryIds` (`parse_entry_ids`), so the
    // list travels sorted LEXICOGRAPHICALLY — with eleven entries "10" comes
    // before "2" — and the selection digest covers exactly that list.
    const entryIds = archive
      ? allEntries.map((each) => String(each.id)).sort()
      : [entry.id];
    // A store-only archive is never SMALLER than the bytes it packs, so the
    // sum is a lower bound and the right thing to ask the quota for.
    const estimate = archive
      ? allEntries.reduce((total, each) => total + Number(each.size ?? 0), 0)
      : Number(entry.size);
    const quota = await repository
      .quota(estimate)
      .catch(() => ({ ok: true, unknown: true }));
    if (quota.ok !== true) {
      events.onError?.(null, "STORAGE_QUOTA");
      return { error: "STORAGE_QUOTA" };
    }
    const selectionDigest = await sha256Hex(
      new TextEncoder().encode(
        canonicalize({
          entryIds,
          manifestMac: macHex,
          mode,
          offerId,
        }),
      ),
    );
    const recordKey = partialKey(
      roomIdHex,
      sourcePeerId,
      offerId,
      selectionDigest,
    );
    const dirSegments = partDirSegments(roomIdHex, offerId, selectionDigest);
    // An explicit restart is the one gesture that discards verified bytes.
    if (fresh === true) {
      try {
        await repository.deleteRecord(recordKey);
        await repository.removeDir(dirSegments);
      } catch {
        /* best effort: planning below simply finds nothing */
      }
    }
    // Resume: rehash every recorded chunk, drop corrupt/truncated ones.
    let verifiedRanges = [];
    let record = null;
    let leaves = [];
    let expectedTuple = null;
    try {
      const stored = await repository.loadRecord(recordKey);
      if (stored !== undefined && stored !== null) {
        if (stored.manifestMac === macHex) {
          const rehashed = archive
            ? await rehashArchiveRecord(recordKey, dirSegments, entry, stored)
            : await rehashRecord(recordKey, dirSegments, entry, stored);
          verifiedRanges = rehashed.ranges;
          record = rehashed.record;
          leaves = rehashed.leaves ?? [];
          expectedTuple = rehashed.expected ?? null;
        } else {
          await repository.deleteRecord(recordKey);
          await repository.removeDir(dirSegments);
        }
      }
    } catch {
      verifiedRanges = [];
      record = null;
      leaves = [];
      expectedTuple = null;
    }
    // The wire carries at most `MAX_RESUME_RANGES`, reduced to the stable
    // contiguous prefix when over the cap; what the source skips is exactly
    // what we told it we hold, so the plan is derived from the SAME list.
    const keptRanges = archive
      ? verifiedPrefix(verifiedRanges)
      : capRanges(coalesceRanges(verifiedRanges));
    const resumeChunks = keptRanges.length === 0 ? 0 : keptRanges[0][1];
    const chunkCount = archive ? 0 : Number(entry.chunkCount);
    // An archive has no chunk count before it exists, so it has no plan
    // either: chunks arrive in order and the index IS the count.
    const { send } = planSend(chunkCount, archive ? [] : keptRanges);
    let expectedBytes = 0;
    if (!archive) {
      for (const index of send) {
        expectedBytes += chunkWindow(Number(entry.size), index).length;
      }
    }
    const transfer = {
      setupAt,
      transferId: null,
      attemptId: null,
      chain: Promise.resolve(),
      offerId,
      sourcePeerId,
      entry,
      archive,
      // Archive only: chunks are cut at exactly `CHUNK_BYTES` and arrive in
      // order, so the index IS the committed count. The leaves feed the
      // rolling root the FINAL frame authenticates, and a resume starts
      // with the leaves of the prefix already on disk — they are part of
      // the root even though their bytes never travel again.
      leaves: archive ? leaves : [],
      archiveChunks: archive ? resumeChunks : 0,
      archiveBytes: archive ? resumeChunks * CHUNK_BYTES : 0,
      // The `(length, chunkCount, root)` the FIRST attempt to reach FINAL
      // authenticated, when there was one. A later attempt that presents a
      // different tuple is a source that changed under a verified partial,
      // and the partial is kept until the user says otherwise.
      expectedTuple,
      resumeChunks: archive ? resumeChunks : 0,
      manifestMac: macHex,
      selectionDigest,
      recordKey,
      dirSegments,
      // For an archive this is the lower-bound estimate above, and it is
      // replaced by the authenticated length at FINAL.
      totalBytes: BigInt(
        archive
          ? Math.max(estimate, Number(expectedTuple?.totalBytes ?? 0))
          : entry.size,
      ),
      chunkCount,
      verifiedRanges: keptRanges,
      // Chunk indexes this attempt expects, in arrival order. A resumed
      // source sends only these, so position N on the wire is `plan[N]`
      // here — never N (V002-F04).
      plan: send,
      planPos: 0,
      expectedBytes,
      record,
      aesKey: null,
      pendingFrames: [],
      // Frames that arrived AHEAD of their place in the stream, keyed by the
      // sequence in their own header; empty on a single transport.
      reorder: new Map(),
      reorderBytes: 0,
      // Armed only by an attempt that runs on MORE THAN ONE carrier, which
      // is the only way frames can arrive out of order.
      reorderWindow: false,
      /** Budget for the held frames, sized from the announced carrier count. */
      reorderLimits: reorderBudget(1),
      /** When the current gap opened, or 0 when nothing is held (B-A040). */
      reorderGapSince: 0,
      /** Direct-path idle clock: handle, and when a frame last arrived. */
      idleTimer: null,
      lastFrameAt: 0,
      /** A direct attempt being negotiated while the relay still carries. */
      upgrade: null,
      // The next sequence to RELEASE into the inbox. Distinct from
      // `expectedSeq`, which is the next sequence to be OPENED: everything
      // between the two is in the inbox, in order, waiting for the pipeline.
      reorderNext: 0,
      inbox: [],
      pumping: false,
      // The current attempt is over and nothing more may be verified under
      // it. Set by `directFailed` (which REPORTS what is on disk) and
      // cleared by `adoptAttempt`: between those two moments the pump must
      // not add a chunk, or the source would be told to skip a range that
      // is not the one the recipient will actually plan around.
      attemptClosed: false,
      socket: null,
      ticket: null,
      // The transport the server COMMITTED, and the path this row may show.
      // They differ until the first chunk is verified: a committed path that
      // has carried nothing is not yet a fact about the transfer.
      transport: "relay",
      path: "connecting",
      pathAttempt: null,
      lastReportAt: 0,
      lastReportBytes: null,
      receivedBytes: 0,
      expectedSeq: 0,
      chunkBuffers: [],
      abort: new AbortController(),
      state: "requesting",
    };
    const resume =
      keptRanges.length > 0
        ? {
            verifiedRanges: keptRanges,
            // A NUMBER: the server parses this field with `as_u64` and
            // rejects the whole request as INVALID_MESSAGE when it arrives
            // as the manifest's decimal string. For an archive the server
            // has no manifest length to bound it against (the archive does
            // not exist yet), so it carries the bytes this peer holds.
            outputLength: archive
              ? resumeChunks * CHUNK_BYTES
              : Number(entry.size),
          }
        : undefined;
    const requestId = randomRequestId();
    const body = {
      offerId,
      entryIds,
      selectionDigest,
      mode,
    };
    if (resume !== undefined) {
      body.resume = resume;
    }
    if (!sendControl({ v: 1, type: "transfer.request", requestId, body })) {
      return { error: "OFFLINE" };
    }
    pendingRequests.set(requestId, transfer);
    transfer.requestTimer = setTimeout(() => {
      pendingRequests.delete(requestId);
    }, 30_000);
    // Keeps its `unref` (B-A034): nothing observes this firing — it drops a
    // map entry no later read reaches — and a reffed 30 s cleanup would add
    // 30 s to the end of every test process that ever sent a request.
    if (typeof transfer.requestTimer?.unref === "function") {
      transfer.requestTimer.unref();
    }
    return { pending: true };
  }

  /**
   * Rehashes every chunk the record claims, dropping the ones whose staged
   * part is missing, short or no longer matches the manifest digest.
   * @returns `{ ranges, record }` — the cleaned ranges and the record as
   * written back (kept in memory from here on).
   */
  async function rehashRecord(recordKey, dirSegments, entry, stored) {
    const kept = [];
    const digests = {};
    const chunkCount = Number(entry.chunkCount);
    for (const [start, end] of coalesceRanges(stored?.verifiedRanges ?? [])) {
      for (let index = start; index < Math.min(end, chunkCount); index++) {
        const { length } = chunkWindow(Number(entry.size), index);
        const bytes = await repository.readChunk(
          dirSegments,
          chunkPartName(entry.id, index),
        );
        if (bytes === null || bytes.length !== length) {
          await repository.removePart(
            dirSegments,
            chunkPartName(entry.id, index),
          );
          continue;
        }
        const digest = await sha256Hex(bytes);
        if (digest !== entry.chunks[index]) {
          await repository.removePart(
            dirSegments,
            chunkPartName(entry.id, index),
          );
          continue;
        }
        kept.push([index, index + 1]);
        digests[index] = digest;
      }
    }
    const cleaned = coalesceRanges(kept);
    const record = {
      ...(stored ?? {}),
      verifiedRanges: cleaned,
      chunkDigests: digests,
      updatedAt: new Date().toISOString(),
    };
    await repository.saveRecord(recordKey, record);
    return { ranges: cleaned, record };
  }

  /**
   * Rehashes an ARCHIVE's staged prefix.
   *
   * There is no manifest digest to check an archive chunk against — the
   * archive is generated — so what a staged part is checked against is the
   * digest THIS peer recorded when it verified and wrote that part. That is
   * weaker than the manifest (it proves the bytes on disk are the bytes we
   * staged, not that the source still produces them) and it does not need
   * to be stronger: the rolling root in the FINAL frame covers every leaf,
   * including the resumed ones, so a source that no longer produces these
   * bytes fails the root and the whole attempt is refused.
   *
   * The prefix ends at the FIRST part that is missing, short or altered.
   * Every archive chunk but the last is exactly `CHUNK_BYTES`, so a short
   * part can only be a last chunk staged by an attempt that then failed its
   * FINAL checks, and resuming past it would put the next arrival one index
   * too high.
   *
   * @returns `{ ranges, leaves, expected, record }`
   */
  async function rehashArchiveRecord(recordKey, dirSegments, entry, stored) {
    const claimed = verifiedPrefix(stored?.verifiedRanges ?? []);
    const end = claimed.length === 0 ? 0 : claimed[0][1];
    const leaves = [];
    const digests = {};
    let kept = 0;
    for (let index = 0; index < end; index++) {
      const name = chunkPartName(entry.id, index);
      const bytes = await repository.readChunk(dirSegments, name);
      const digest =
        bytes === null || bytes.length !== CHUNK_BYTES
          ? null
          : await sha256Hex(bytes);
      if (digest === null || digest !== (stored?.chunkDigests ?? {})[index]) {
        break;
      }
      leaves.push(hexToBytes(digest));
      digests[index] = digest;
      kept = index + 1;
    }
    // Everything past the prefix is unusable: an archive resumes on a
    // prefix alone, so keeping those parts would only occupy the quota.
    for (let index = kept; index < end; index++) {
      await repository.removePart(dirSegments, chunkPartName(entry.id, index));
    }
    const ranges = kept === 0 ? [] : [[0, kept]];
    const expected = archiveExpected(stored?.expected);
    const record = {
      ...(stored ?? {}),
      verifiedRanges: ranges,
      chunkDigests: digests,
      size: kept * CHUNK_BYTES,
      updatedAt: new Date().toISOString(),
    };
    await repository.saveRecord(recordKey, record);
    return { ranges, leaves, expected, record };
  }

  /**
   * The stored `(length, chunkCount, root)` of an archive, or `null` when
   * no attempt has ever reached FINAL. A record whose tuple is malformed is
   * read as absent rather than trusted: it was written by this code, so a
   * shape it does not produce is a corrupted store, and demanding a match
   * against garbage would strand a partial forever.
   */
  function archiveExpected(value) {
    if (
      value === null ||
      typeof value !== "object" ||
      !Number.isSafeInteger(value.totalBytes) ||
      !Number.isSafeInteger(value.chunkCount) ||
      typeof value.root !== "string" ||
      !/^[0-9a-f]{64}$/.test(value.root)
    ) {
      return null;
    }
    return {
      totalBytes: value.totalBytes,
      chunkCount: value.chunkCount,
      root: value.root,
    };
  }

  /** Derives the decrypt-only attempt key; queued frames replay in order. */
  async function deriveAttemptKey(transfer) {
    const { signal } = transfer.abort;
    try {
      const keyBytes = await attemptKey(
        hexToBytes(roomKeyHex),
        hexToBytes(transfer.transferId),
        hexToBytes(transfer.attemptId),
      );
      if (signal.aborted) {
        return;
      }
      transfer.aesKey = await importAttemptAesKey(keyBytes, ["decrypt"]);
      keyBytes.fill(0);
      const queued = transfer.pendingFrames;
      transfer.pendingFrames = [];
      for (const data of queued) {
        if (signal.aborted) {
          return;
        }
        deliverFrame(transfer, data);
      }
      await transfer.chain;
    } catch (error) {
      failTransfer(transfer.transferId, "FAILED", `derive: ${error?.message}`);
    }
  }

  function openRelay(transfer) {
    const socket = createSocket(
      `${relayBase}/transfer/ws/relay/${roomIdHex}/${transfer.transferId}`,
      "bore-transfer-v1",
    );
    transfer.socket = socket;
    // Frames are binary and are opened one by one, so taking them as
    // ArrayBuffer costs nothing and skips a Blob per message. MEASURED
    // (3.10, 32 MiB over 1377 frames): the `await blob.arrayBuffer()` hop
    // alone was 333 ms of an 801 ms transfer — 37%, the single largest
    // stage, larger than the cipher and the staging layer together.
    socket.binaryType = "arraybuffer";
    socket.onopen = () => {
      if (transfer.ticket === null) {
        return;
      }
      try {
        socket.send(
          JSON.stringify({
            v: 1,
            peerId: getSelfPeerId(),
            transferId: transfer.transferId,
            attemptId: transfer.attemptId,
            role: "recipient",
            ticket: transfer.ticket,
          }),
        );
      } catch (error) {
        failTransfer(
          transfer.transferId,
          "FAILED",
          `attach-send: ${error?.message}`,
        );
      }
    };
    // The leg belongs to ONE attempt: a socket that outlives its attempt
    // must neither feed the next one nor fail it.
    const attempt = transfer.attemptId;
    socket.onmessage = (event) => {
      const live = transfers.get(transfer.transferId);
      if (live === undefined || live.attemptId !== attempt) {
        return;
      }
      deliverFrame(live, event.data);
    };
    socket.onclose = () => {
      const known = transfers.get(transfer.transferId);
      if (known === undefined || known.attemptId !== attempt) {
        return;
      }
      // Decide only after the processing chain drains: the server closes
      // right after FINAL, while queued frames may still be decrypting.
      // Orderly close after FINAL is the normal end; anything earlier is a
      // failure the server already reported (or will) on control.
      known.chain = known.chain.then(() => {
        const current = transfers.get(transfer.transferId);
        if (current === undefined || current.attemptId !== attempt) {
          return;
        }
        if (
          current.state !== "complete-pending" &&
          current.state !== "staged"
        ) {
          failTransfer(transfer.transferId, "FAILED", "relay-close-early");
        }
      });
    };
    socket.onerror = () => {
      // The close event follows and carries the decision.
    };
  }

  /**
   * Queues one arrived frame and makes sure the pump is running. Frames are
   * OPENED several at a time and PROCESSED strictly in order — see
   * {@link pumpFrames}.
   */
  /**
   * True when the counterpart has already written FINAL — the frame is
   * queued but not yet opened. That is the ONE case where the close of a
   * transport is the end of a SUCCESSFUL transfer rather than a failure, so
   * it is the one case worth waiting for the pipeline over. Read off the
   * cleartext header; an unreadable frame counts as FINAL, because waiting
   * for nothing costs a moment and tearing down a finished transfer costs
   * the transfer.
   */
  function finalIsQueued(transfer) {
    for (const queue of [transfer.inbox, transfer.pendingFrames]) {
      if (queue.length === 0) {
        continue;
      }
      const type = peekFrameType(queue[queue.length - 1]);
      if (type === null || type === FRAME_FINAL) {
        return true;
      }
    }
    return false;
  }

  function deliverFrame(transfer, data) {
    if (transfer.attemptClosed) {
      // The attempt is over: a frame the transport had already queued
      // belongs to a world that ended, and opening it would advance this
      // attempt's sequence for nothing.
      return;
    }
    if (transfer.aesKey === null) {
      // Key still deriving: queue in arrival order, replayed on readiness.
      transfer.pendingFrames.push(data);
      return;
    }
    if (!admitInOrder(transfer, data)) {
      return;
    }
    if (transfer.pumping) {
      return;
    }
    transfer.pumping = true;
    transfer.chain = transfer.chain.then(() => pumpFrames(transfer));
  }

  /**
   * Puts one frame in its place in the stream, releasing into the inbox
   * everything that is now contiguous. Returns false when nothing was
   * released — the caller then has no pump to start.
   *
   * The sequence is read from the frame's own cleartext header, which the
   * AEAD authenticates, and is used ONLY to decide where the frame goes. The
   * authority is unchanged: `pumpFrames` still opens the Nth released frame
   * against sequence N, so a frame put in the wrong place fails to open
   * exactly as it did before this existed. That is what makes reordering
   * free of any security consequence — a peer that lies about a sequence can
   * cost itself a failed attempt and nothing else.
   *
   * A frame whose header cannot be read here (a `Blob`, which no transport of
   * ours produces) is taken as the next one in order, which is precisely the
   * behaviour of every version before carriers existed.
   */
  function admitInOrder(transfer, data) {
    if (!transfer.reorderWindow) {
      // ONE transport delivers the frame stream in the order it was sent, so
      // there is nothing to reorder and a gap is not skew — it is a stream
      // that no longer means what this attempt assumes. This is the path the
      // relay leg and a single-carrier direct attempt take, and it is exactly
      // the code that existed before carriers: the frame goes straight to the
      // inbox and `pumpFrames` assigns the sequence, so a gap, a duplicate or
      // a reorder fails the open just as it always did.
      transfer.inbox.push(data);
      return true;
    }
    const seq = peekFrameSeq(data);
    if (seq === null || seq === transfer.reorderNext) {
      transfer.inbox.push(data);
      transfer.reorderNext += 1;
      // The gap just closed, so the deadline starts again from whatever
      // opens the NEXT one. A window that keeps draining never trips it.
      transfer.reorderGapSince = 0;
      // Whatever was waiting on this frame can go now, and so can whatever
      // was waiting on THAT — a single carrier catching up releases its whole
      // run in one pass.
      while (transfer.reorder.has(transfer.reorderNext)) {
        const next = transfer.reorder.get(transfer.reorderNext);
        transfer.reorder.delete(transfer.reorderNext);
        transfer.reorderBytes -= frameByteLength(next);
        transfer.inbox.push(next);
        transfer.reorderNext += 1;
      }
      return true;
    }
    if (seq < transfer.reorderNext || transfer.reorder.has(seq)) {
      // Already released, or already held: a duplicate is not a reorder. On
      // a reliable transport it cannot happen at all, so it is a stream that
      // no longer means what this attempt assumes.
      failAttempt(transfer, "protocol");
      return false;
    }
    transfer.reorder.set(seq, data);
    transfer.reorderBytes += frameByteLength(data);
    // How close the window came to its ceiling is the only way to tell a
    // skew the window absorbs from one it is about to refuse, and the
    // overflow itself reports neither.
    perfPeak("dst.reorder.bytes", transfer.reorderBytes);
    perfPeak("dst.reorder.frames", transfer.reorder.size);
    if (transfer.reorderGapSince === 0) {
      // The gap opened now. Timing it from the FIRST held frame, and not
      // from every later one, is what makes the deadline measure the gap
      // rather than the traffic still arriving past it.
      transfer.reorderGapSince = Date.now();
    }
    const budget = transfer.reorderLimits ?? reorderBudget(1);
    if (
      transfer.reorder.size > budget.frames ||
      transfer.reorderBytes > budget.bytes
    ) {
      // The window cannot hold more. It lives in the tab's own heap, so
      // there is nothing else to do but abandon the attempt.
      failAttempt(transfer, "stalled");
      return false;
    }
    if (reorderStallMs > 0 && Date.now() - transfer.reorderGapSince >= reorderStallMs) {
      // The gap has not filled inside the deadline: a carrier that is behind
      // advances this, and one that has stopped does not. THIS is the test
      // the byte ceiling used to stand in for.
      failAttempt(transfer, "stalled");
      return false;
    }
    return false;
  }

  /** Byte length of a frame however the transport delivered it. */
  function frameByteLength(data) {
    if (data instanceof ArrayBuffer) {
      return data.byteLength;
    }
    if (ArrayBuffer.isView(data)) {
      return data.byteLength;
    }
    return Number(data?.size ?? 0);
  }

  /**
   * Ends the current attempt without ending the transfer. The caller upward
   * turns this into the `transfer.direct_failed` the server answers with a
   * relay attempt; a receiver with no such caller simply stops feeding a
   * transport it cannot follow.
   */
  function failAttempt(transfer, reason) {
    if (transfer.attemptClosed) {
      return;
    }
    stopIdleClock(transfer);
    transfer.inbox = [];
    transfer.reorder = new Map();
    transfer.reorderBytes = 0;
    events.onAttemptFailed?.(transfer.transferId, transfer.attemptId, reason);
  }

  /** As `Uint8Array`, whatever the socket delivered (or a test handed us). */
  async function frameBytes(data) {
    const bytesAt = perfStart();
    const bytes =
      data instanceof ArrayBuffer
        ? new Uint8Array(data)
        : ArrayBuffer.isView(data)
          ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
          : new Uint8Array(await data.arrayBuffer());
    perfEnd("dst.bytes", bytesAt, bytes.length);
    return bytes;
  }

  /**
   * Drains the inbox in batches: every frame in a batch is opened at once,
   * and the results are consumed in arrival order.
   *
   * Opening several at once is what the engines reward. MEASURED in-page
   * (3.10, 1376 x 24 KiB AES-GCM opens): one at a time reads 1008 MiB/s on
   * webkit and 921 on firefox, eight at a time 2481 and 1897 — 2.5x and 2.1x
   * — while chromium is flat (1979 against 2122), so nobody pays for it.
   *
   * Nothing about the verification moves: each frame is still opened against
   * its OWN expected sequence, assigned here in arrival order, so a gap, a
   * replay or a reordered frame fails exactly as before; and the plaintext
   * is still consumed one frame at a time, in order, by the same code.
   */
  async function pumpFrames(transfer) {
    const { signal } = transfer.abort;
    // The attempt this pump belongs to. Every batch — and every plaintext
    // inside one — is checked against it before anything is written, so a
    // batch that was already in flight when the attempt died cannot write
    // a chunk, advance the plan or extend the verified ranges of the
    // attempt that replaced it.
    const attempt = transfer.attemptId;
    const stale = () =>
      transfer.attemptClosed || transfer.attemptId !== attempt;
    try {
      while (transfer.inbox.length > 0) {
        if (signal.aborted || stale()) {
          return;
        }
        const batch = transfer.inbox.splice(0, FRAME_PIPELINE_DEPTH);
        if (transfer.firstFrameAt === undefined) {
          transfer.firstFrameAt = perfStart();
          // Everything between the click and the first frame this peer
          // opens: quota, resume record, the request round trip, the key.
          perfEnd("dst.setup", transfer.setupAt);
        }
        const busyAt = perfStart();
        let batchBytes = 0;
        const opens = [];
        for (const data of batch) {
          const seq = transfer.expectedSeq;
          transfer.expectedSeq += 1;
          opens.push(
            frameBytes(data).then((bytes) => {
              batchBytes += bytes.length;
              const openAt = perfStart();
              return decodeFrameWithKey(transfer.aesKey, bytes, seq).then(
                (opened) => {
                  perfEnd("dst.open", openAt, bytes.length);
                  return opened;
                },
              );
            }),
          );
        }
        // Every open is awaited even when an earlier one failed: a rejection
        // nobody awaits is an unhandled rejection, and the first failure is
        // the one that must be reported.
        const settled = await Promise.allSettled(opens);
        for (const result of settled) {
          if (result.status === "rejected") {
            throw result.reason;
          }
        }
        for (const result of settled) {
          if (signal.aborted || stale()) {
            return;
          }
          const opened = result.value;
          if (opened.ftype === FRAME_DATA) {
            await acceptData(transfer, opened.plaintext, stale);
          } else if (opened.ftype === FRAME_FINAL) {
            await acceptFinal(transfer, opened.plaintext, stale);
          }
        }
        perfEnd("dst.busy", busyAt, batchBytes);
      }
    } catch (error) {
      if (!stale()) {
        // A `code` on the error is the pipeline's way of naming a failure
        // the UI must treat differently — `SOURCE_CHANGED` is the one that
        // must NOT purge the partial, because the bytes on disk are still
        // the ones this peer verified.
        failTransfer(transfer.transferId, error?.code ?? "FAILED", error?.message);
      }
    } finally {
      if (!stale()) {
        transfer.pumping = false;
      }
    }
  }

  /**
   * @param {object} transfer live download state
   * @param {Uint8Array} plaintext one opened DATA frame
   * @param {() => boolean} stale true once this frame's attempt is over —
   * checked after every await, because `planPos` and `chunkBuffers` are the
   * NEXT attempt's receive position by then and advancing them would make
   * the two ends disagree about which chunk is on the wire.
   */
  async function acceptData(transfer, plaintext, stale = () => false) {
    // Fragments arrive in chunk order; a chunk commits to disk only whole,
    // after its digest matches the manifest. Which chunk that is comes from
    // the plan, so a source that skips resumed ranges still lands correctly.
    if (stale()) {
      return;
    }
    if (transfer.archive) {
      await acceptArchiveData(transfer, plaintext, stale);
      return;
    }
    if (transfer.planPos >= transfer.plan.length) {
      throw new Error("fragment past the entry end");
    }
    const chunkIndex = transfer.plan[transfer.planPos];
    const { length } = chunkWindow(Number(transfer.entry.size), chunkIndex);
    transfer.chunkBuffers.push(plaintext);
    transfer.receivedBytes += plaintext.length;
    let buffered = 0;
    for (const part of transfer.chunkBuffers) {
      buffered += part.length;
    }
    if (buffered < length) {
      reportProgress(transfer);
      return;
    }
    if (buffered > length) {
      throw new Error("fragment overrun");
    }
    const joinAt = perfStart();
    const chunk = new Uint8Array(length);
    let at = 0;
    for (const part of transfer.chunkBuffers) {
      chunk.set(part, at);
      at += part.length;
    }
    transfer.chunkBuffers = [];
    perfEnd("dst.join", joinAt, length);
    const digestAt = perfStart();
    const digest = await sha256Hex(chunk);
    perfEnd("dst.digest", digestAt, length);
    if (stale()) {
      // The attempt ended while this chunk was being hashed. Nothing has
      // been written and nothing is recorded, so the replacement attempt
      // simply receives it again.
      return;
    }
    if (digest !== transfer.entry.chunks[chunkIndex]) {
      throw new Error("chunk digest mismatch");
    }
    const stageAt = perfStart();
    await repository.writeChunk(
      transfer.dirSegments,
      chunkPartName(transfer.entry.id, chunkIndex),
      chunk,
    );
    perfEnd("dst.stage", stageAt, length);
    if (stale()) {
      // The part is on disk and verified, but this attempt no longer owns
      // the plan: leaving it unrecorded costs one re-receive and keeps the
      // two ends agreeing on what the next attempt sends.
      return;
    }
    transfer.verifiedRanges = coalesceRanges([
      ...transfer.verifiedRanges,
      [chunkIndex, chunkIndex + 1],
    ]);
    // The record is carried in memory: reading it back before every write
    // costs a transaction per chunk and adds nothing we do not already know.
    transfer.record = {
      manifestMac: transfer.manifestMac,
      roomId: roomIdHex,
      offerId: transfer.offerId,
      digest: transfer.selectionDigest,
      kind: "file",
      path: transfer.entry.path,
      size: transfer.entry.size,
      root: transfer.entry.root,
      verifiedRanges: transfer.verifiedRanges,
      chunkDigests: {
        ...((transfer.record ?? {}).chunkDigests ?? {}),
        [chunkIndex]: digest,
      },
      updatedAt: new Date().toISOString(),
    };
    const commitAt = perfStart();
    await repository.saveRecord(transfer.recordKey, transfer.record);
    perfEnd("dst.commit", commitAt, length);
    if (stale()) {
      return;
    }
    transfer.planPos += 1;
    // The FIRST chunk THIS ATTEMPT verified is what makes its transport real:
    // until then the path is committed but has carried nothing. Keyed by
    // attempt, so a fallback's badge flips to `relay` on its own first
    // verified chunk and not one moment earlier.
    nameTransport(transfer);
    reportProgress(transfer, true);
  }

  /**
   * One DATA frame of a ZIP transfer. There is no manifest entry for the
   * archive and therefore no per-chunk digest to check against: what
   * authenticates these bytes is the frame's own AEAD, sealed under the
   * attempt key, and the rolling root over every leaf, checked at FINAL
   * against the tuple the source sealed. Chunks are cut at exactly
   * `CHUNK_BYTES`; the short last one is committed by `acceptArchiveFinal`,
   * because the frame that carries it cannot say that it is the last.
   */
  async function acceptArchiveData(transfer, plaintext, stale) {
    transfer.chunkBuffers.push(plaintext);
    transfer.receivedBytes += plaintext.length;
    let buffered = 0;
    for (const part of transfer.chunkBuffers) {
      buffered += part.length;
    }
    if (buffered < CHUNK_BYTES) {
      reportProgress(transfer);
      return;
    }
    if (buffered > CHUNK_BYTES) {
      throw new Error("fragment overrun");
    }
    await commitArchiveChunk(transfer, stale);
  }

  /**
   * Stages whatever the archive has buffered as the next chunk. Called with
   * a full `CHUNK_BYTES` from `acceptArchiveData` and with the remainder
   * from `acceptArchiveFinal`; an empty buffer commits nothing, which is the
   * ordinary case for an archive whose length is a multiple of the chunk.
   */
  async function commitArchiveChunk(transfer, stale) {
    let length = 0;
    for (const part of transfer.chunkBuffers) {
      length += part.length;
    }
    if (length === 0) {
      return;
    }
    const joinAt = perfStart();
    const chunk = new Uint8Array(length);
    let at = 0;
    for (const part of transfer.chunkBuffers) {
      chunk.set(part, at);
      at += part.length;
    }
    transfer.chunkBuffers = [];
    perfEnd("dst.join", joinAt, length);
    const digestAt = perfStart();
    const digest = await sha256Hex(chunk);
    perfEnd("dst.digest", digestAt, length);
    if (stale()) {
      return;
    }
    const index = transfer.archiveChunks;
    const stageAt = perfStart();
    await repository.writeChunk(
      transfer.dirSegments,
      chunkPartName(transfer.entry.id, index),
      chunk,
    );
    perfEnd("dst.stage", stageAt, length);
    if (stale()) {
      return;
    }
    transfer.leaves.push(hexToBytes(digest));
    transfer.archiveChunks = index + 1;
    transfer.archiveBytes += length;
    // The archive's own resume baseline, and NOT only the record's: it is
    // what `directFailed` reports to the server, what `replanArchive`
    // measures the commit against, and what the progress row reads. Leaving
    // it at the value `startDownload` planned made a mid-transfer fallback
    // throw away every chunk the dead attempt had verified, and reported a
    // transfer that had staged megabytes as holding nothing.
    transfer.verifiedRanges = [[0, transfer.archiveChunks]];
    transfer.record = {
      manifestMac: transfer.manifestMac,
      roomId: roomIdHex,
      offerId: transfer.offerId,
      digest: transfer.selectionDigest,
      kind: "zip",
      path: transfer.entry.path,
      size: transfer.archiveBytes,
      root: transfer.expectedTuple?.root ?? null,
      // Carried forward on every write so an interrupted attempt leaves it
      // on disk: it is what a later attempt is held to.
      expected: transfer.expectedTuple,
      verifiedRanges: [[0, transfer.archiveChunks]],
      chunkDigests: {
        ...((transfer.record ?? {}).chunkDigests ?? {}),
        [index]: digest,
      },
      updatedAt: new Date().toISOString(),
    };
    const commitAt = perfStart();
    await repository.saveRecord(transfer.recordKey, transfer.record);
    perfEnd("dst.commit", commitAt, length);
    if (stale()) {
      return;
    }
    nameTransport(transfer);
    reportProgress(transfer, true);
  }

  /** Verified bytes over the whole entry, resumed chunks included. */
  function reportProgress(transfer, verifiedChunk = false) {
    let verified = 0;
    if (transfer.archive) {
      verified = transfer.archiveBytes;
    } else {
      for (const [start, end] of transfer.verifiedRanges) {
        for (let index = start; index < end; index++) {
          verified += chunkWindow(Number(transfer.entry.size), index).length;
        }
      }
    }
    events.onProgress?.({
      transferId: transfer.transferId,
      receivedBytes: verified,
      // An archive's total is an estimate until FINAL, and an estimate that
      // has already been passed is not a total: report whichever is larger,
      // so the bar never runs backwards or past its own end.
      totalBytes: transfer.archive
        ? Math.max(Number(transfer.totalBytes), verified)
        : Number(transfer.totalBytes),
      path: transfer.path,
    });
    if (!verifiedChunk) {
      // Fragments of a chunk that is not complete prove nothing yet: the
      // source hears only about bytes this peer VERIFIED against the
      // manifest, which is also what makes the path a fact (4.3/D3).
      return;
    }
    const now = Date.now();
    const first = transfer.lastReportBytes === null;
    if (!progressIsDue(now, transfer.lastReportAt, first, transfers.size)) {
      return;
    }
    transfer.lastReportAt = now;
    transfer.lastReportBytes = verified;
    sendControl({
      v: 1,
      type: "transfer.progress",
      requestId: randomRequestId(),
      body: {
        transferId: transfer.transferId,
        attemptId: transfer.attemptId,
        // A decimal string, like every other 64-bit quantity on this wire.
        receivedBytes: String(verified),
      },
    });
  }

  /**
   * FINAL of a ZIP transfer: the `(length, chunkCount, root)` tuple the
   * source sealed. None of it is in the manifest — the archive is generated
   * — so this frame is where the recipient learns what it should have
   * received, and the AEAD is what makes that claim the source's own. The
   * short last chunk is committed here, because only this frame says that
   * no more are coming.
   *
   * @returns false when the attempt ended mid-commit; the replacement
   * attempt receives the archive again from chunk zero.
   */
  async function acceptArchiveFinal(transfer, plaintext, stale) {
    const { totalBytes, chunkCount, root } =
      parseArchiveFinalPayload(plaintext);
    await commitArchiveChunk(transfer, stale);
    if (stale()) {
      return false;
    }
    // The tuple describes the WHOLE archive, so it is checked against what
    // this peer holds on disk — not against what arrived on this attempt,
    // which on a resume is only the part the source did not skip.
    if (transfer.archiveBytes !== totalBytes) {
      throw new Error("FINAL total mismatch");
    }
    if (transfer.archiveChunks !== chunkCount) {
      throw new Error("FINAL chunk count mismatch");
    }
    const rootHex = bytesToHex(
      await fileRoot(transfer.leaves.length, transfer.leaves),
    );
    const sourceChanged = (message) =>
      Object.assign(new Error(message), { code: "SOURCE_CHANGED" });
    if (rootHex !== bytesToHex(root)) {
      // The leaves of a resumed prefix come from THIS peer's disk and the
      // rest from the wire, so a root the source sealed over its own
      // regeneration can only disagree if the files behind the offer no
      // longer produce the archive that was half received. Nothing is
      // deleted: the verified bytes stay until the user asks for a restart.
      throw transfer.resumeChunks > 0
        ? sourceChanged("archive root does not match the resumed partial")
        : new Error("archive root mismatch");
    }
    const previous = transfer.expectedTuple;
    if (
      previous !== null &&
      previous !== undefined &&
      (previous.totalBytes !== totalBytes ||
        previous.chunkCount !== chunkCount ||
        previous.root !== rootHex)
    ) {
      throw sourceChanged("archive differs from the one already verified");
    }
    transfer.expectedTuple = { totalBytes, chunkCount, root: rootHex };
    if (transfer.record !== null && transfer.record !== undefined) {
      transfer.record = {
        ...transfer.record,
        root: rootHex,
        expected: transfer.expectedTuple,
      };
      try {
        await repository.saveRecord(transfer.recordKey, transfer.record);
      } catch {
        // The bytes and their digests are already durable; losing this one
        // write costs a re-download, never a wrong resume.
      }
    }
    // Only now is the archive a thing with a size: everything downstream
    // (staging, the completion request, the UI) reads these fields, so they
    // are filled from the tuple that was just authenticated, never earlier.
    transfer.totalBytes = BigInt(totalBytes);
    transfer.chunkCount = chunkCount;
    transfer.entry = {
      ...transfer.entry,
      size: totalBytes,
      chunkCount,
      root: rootHex,
    };
    return true;
  }

  async function acceptFinal(transfer, plaintext, stale = () => false) {
    if (stale()) {
      return;
    }
    if (transfer.archive) {
      if ((await acceptArchiveFinal(transfer, plaintext, stale)) !== true) {
        return;
      }
    } else {
      if (plaintext.length !== 8) {
        throw new Error("bad FINAL length");
      }
      const total = new DataView(
        plaintext.buffer,
        plaintext.byteOffset,
        8,
      ).getBigUint64(0, false);
      // FINAL counts the bytes that were meant to travel on THIS attempt,
      // which on a resume is only the chunks the source did not skip.
      if (
        total !== BigInt(transfer.expectedBytes) ||
        transfer.receivedBytes !== transfer.expectedBytes
      ) {
        throw new Error("FINAL total mismatch");
      }
      const covered = transfer.verifiedRanges;
      const complete =
        transfer.chunkCount === 0
          ? covered.length === 0
          : covered.length === 1 &&
            covered[0][0] === 0 &&
            covered[0][1] === transfer.chunkCount;
      if (!complete) {
        throw new Error("verified ranges do not cover the entry");
      }
    }
    // Wall clock from the first frame to the last: with `dst.busy` beside it,
    // the difference is time this peer spent waiting for the wire.
    perfEnd("dst.wall", transfer.firstFrameAt, transfer.receivedBytes);
    transfer.completeAt = perfStart();
    transfer.state = "complete-pending";
    // Past the last byte the recipient waits for the server on purpose.
    stopIdleClock(transfer);
    // The server notifies only the source on completion; the recipient
    // learns it from this request's ack, tracked below.
    if (
      !sendComplete(transfer.transferId, {
        transferId: transfer.transferId,
        attemptId: transfer.attemptId,
        root: transfer.entry.root,
      })
    ) {
      throw new Error("offline at complete");
    }
  }

  /**
   * Puts one `transfer.complete` on the wire and remembers it so a refusal
   * can be answered. Each send gets a FRESH `requestId`: the server answers
   * per request, so reusing one would only collect the first verdict again.
   */
  function sendComplete(transferId, body, tries = 0) {
    const completeId = randomRequestId();
    const sent = sendControl({
      v: 1,
      type: "transfer.complete",
      requestId: completeId,
      body,
    });
    if (!sent) {
      return false;
    }
    pendingCompletes.set(completeId, { transferId, body, tries });
    return true;
  }

  /**
   * A refused `transfer.complete`. A retryable code is re-sent on a growing
   * grid; anything else, and a spent grid, fails the transfer with the code
   * the server gave — the user sees why, instead of a row stuck at 100%.
   */
  function completeRefused(pending, code) {
    const retryable = code === "RATE_LIMITED" || code === "INTERNAL";
    if (!retryable || pending.tries >= COMPLETE_MAX_RETRIES) {
      events.onError?.(pending.transferId, code);
      return;
    }
    const wait = COMPLETE_RETRY_BASE_MS * 2 ** pending.tries;
    const timer = setTimeout(() => {
      // The transfer may have been abandoned while we waited.
      if (!transfers.has(pending.transferId)) {
        return;
      }
      sendComplete(pending.transferId, pending.body, pending.tries + 1);
    }, wait);
    // Reffed on purpose (B-A034): the expiry of this backoff IS the retry, so
    // a test waiting for the next `transfer.complete` must not have the
    // runtime declare the loop idle underneath it. See `sink.waitLow` in
    // `webrtc.js` for the rule and the failure it came from.
  }

  /** Stages the verified file after the server echoes completion. */
  async function stageCompleted(transferId) {
    const transfer = transfers.get(transferId);
    if (transfer === undefined) {
      return;
    }
    try {
      const names = [];
      for (let index = 0; index < transfer.chunkCount; index++) {
        names.push(chunkPartName(transfer.entry.id, index));
      }
      // The parts stay where they are; the Blob reads them on demand, so
      // staging costs no copy and no second output file.
      const ackAt = perfStart();
      perfEnd("dst.ack", transfer.completeAt);
      const blobAt = perfStart();
      const blob = await repository.stagedBlob(transfer.dirSegments, names);
      perfEnd("dst.blob", blobAt, Number(transfer.entry.size));
      if (blob.size !== Number(transfer.entry.size)) {
        throw new Error("staged size mismatch");
      }
      const urlAt = perfStart();
      const url = globalThis.URL.createObjectURL(blob);
      perfEnd("dst.url", urlAt);
      perfEnd("dst.tail", ackAt, Number(transfer.entry.size));
      // The whole recipient path on the page's own clock: click to staged
      // file. The harness reports THIS, because a DOM poll grid quantizes
      // to 100/250/500 ms and at 32 MiB the grid, not the pipeline, decided
      // the number (3.10).
      perfEnd("dst.e2e", transfer.setupAt, Number(transfer.entry.size));
      stagedFiles.set(transferId, {
        transferId,
        offerId: transfer.offerId,
        fileName: transfer.entry.path,
        size: blob.size,
        blob,
        url,
        dirSegments: transfer.dirSegments,
        recordKey: transfer.recordKey,
      });
      transfer.state = "staged";
      events.onStaged?.({
        transferId,
        fileName: transfer.entry.path,
        url,
        bytes: blob.size,
      });
    } catch (error) {
      failTransfer(transferId, "FAILED", error?.message);
      return;
    }
    // The staged file outlives the transfer: drop the live state (socket
    // already closed, key abandoned) and keep only the staged entry.
    forget(transferId);
  }

  const receiver = {
    transfers() {
      return transfers;
    },

    staged() {
      return stagedFiles;
    },

    /**
     * The server opened a direct attempt for this transfer. The attempt ID
     * and its key are adopted NOW — deriving during ICE, not after it, is
     * what keeps the first frame from waiting on WebCrypto — and the frames
     * arrive through `deliverDirectFrame`.
     */
    beginDirect(transferId, attemptId, carriers = 1) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || typeof attemptId !== "string") {
        return false;
      }
      if (transfer.state !== "requested") {
        return false;
      }
      adoptAttempt(transfer, attemptId);
      // More than one carrier means the arrival order is the order N
      // independent associations happened to deliver in, so the receiver
      // reorders on the sequence in each frame's own header. With one
      // carrier the window stays disarmed and the path is the one that
      // existed before carriers, down to which failures are which.
      transfer.reorderWindow = Number(carriers) > 1;
      // The budget is sized from the count the server announced, because the
      // skew the window must absorb is what the OTHER carriers deliver while
      // one is paused (B-A040).
      transfer.reorderLimits = reorderBudget(carriers);
      transfer.reorderGapSince = 0;
      transfer.state = "direct";
      startIdleClock(transfer);
      void deriveAttemptKey(transfer);
      return true;
    },

    /**
     * A direct attempt to be negotiated WHILE the relay keeps carrying.
     *
     * Nothing about the live attempt moves: not the ID, not the key, not the
     * plan. The relay is still delivering frames and must go on delivering
     * them, because a probe that fails has to leave the download exactly as
     * it found it. The switch happens in one place only — the server's
     * `transfer.path_commit direct` naming this attempt.
     */
    prepareUpgrade(transferId, attemptId, carriers = 1) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || typeof attemptId !== "string") {
        return false;
      }
      if (transfer.attemptId === attemptId) {
        return false;
      }
      // Only a download that is actually running on the relay is worth
      // upgrading; anything else already has a path or has none to leave.
      if (transfer.state !== "receiving" && transfer.state !== "ticketed") {
        return false;
      }
      transfer.upgrade = { attemptId, carriers: Number(carriers) || 1 };
      return true;
    },

    /**
     * What this peer has VERIFIED on disk, for the upgrade's
     * `transfer.direct_ready`. The server relayed the bytes but never counted
     * them, so without this the commit would resend everything the relay had
     * already delivered.
     */
    upgradeRanges(transferId, attemptId) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || transfer.upgrade?.attemptId !== attemptId) {
        return null;
      }
      return transfer.verifiedRanges.map((range) => [...range]);
    },

    /** The probe is over; the relay was never touched. */
    abandonUpgrade(transferId, attemptId) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || transfer.upgrade?.attemptId !== attemptId) {
        return false;
      }
      transfer.upgrade = null;
      return true;
    },

    /** One ciphertext frame off the DataChannel of the current attempt. */
    deliverDirectFrame(transferId, attemptId, data) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || transfer.attemptId !== attemptId) {
        return false;
      }
      if (transfer.state !== "direct" && transfer.state !== "receiving") {
        return false;
      }
      // The clock reads arrival, not progress: a frame that turns out to be
      // unopenable still proves the path is carrying, and it is the path this
      // deadline is about.
      transfer.lastFrameAt = Date.now();
      deliverFrame(transfer, data);
      return true;
    },

    /**
     * The direct transport for this transfer is gone. What is on disk stays
     * (it is verified by construction) and is reported back to the server so
     * the relay attempt skips it; the caller puts these ranges in
     * `transfer.direct_failed`.
     * A channel that closes right after FINAL is the END of a successful
     * transfer, not a failure — and the frames it already delivered may still
     * be decrypting when the close event fires. So the decision is taken
     * ONLY after the processing chain drains. The relay leg has had this
     * rule since it was written (`socket.onclose`); the direct leg did not,
     * and answering immediately cleared `inbox` — throwing away the FINAL of
     * a transfer whose every byte was already verified on disk. The row then
     * sat at `100% · transferring` for ever, with the badge walked back to
     * `connecting`, and only a reload and a resume could finish it.
     *
     * @returns a promise of the bounded verified ranges, or `null` when
     * there is nothing to report: an unknown transfer, an attempt already
     * reported, or a transfer that COMPLETED while we waited for the drain.
     */
    async directFailed(transferId, attemptId) {
      const opened = transfers.get(transferId);
      if (opened === undefined || opened.attemptId !== attemptId) {
        return null;
      }
      // The wait is NOT unconditional, and that is the whole design. A
      // transport that dies MID-transfer must be reported at once: the
      // ranges are what the server puts in the relay attempt's commit, and
      // both ends notice the same dead channel, so a recipient that pauses
      // to hash a chunk loses the race to the source — whose notice carries
      // no ranges at all — and the replacement attempt re-sends bytes that
      // were already on disk. Waiting is right ONLY where the counterpart
      // has already written FINAL.
      if (finalIsQueued(opened) || opened.state === "complete-pending") {
        // Appended to the chain rather than awaited as a snapshot, so work
        // queued while we wait is covered too — exactly what the relay leg
        // does with `known.chain = known.chain.then(...)`.
        const drained = opened.chain.then(
          () => {},
          () => {},
        );
        opened.chain = drained;
        await drained;
      }
      const transfer = transfers.get(transferId);
      if (transfer === undefined || transfer.attemptId !== attemptId) {
        return null;
      }
      if (
        transfer.state === "complete-pending" ||
        transfer.state === "staged"
      ) {
        return null;
      }
      if (transfer.attemptClosed) {
        // This attempt has already been reported. BOTH ends observe the same
        // dead channel — our own close event and the counterpart's forwarded
        // notice — and which one arrives first is decided by two engines'
        // timers, so a second call here is the ORDINARY case and not an
        // error. Answering `null` is what makes the report exactly-once:
        // the ranges went out with the first notice, and a duplicate would
        // be a second `transfer.direct_failed` for an attempt the server has
        // already replaced.
        return null;
      }
      // Closing FIRST is what makes the answer true: the ranges returned
      // here are what the source is told to skip, and a chunk verified
      // after the report would leave the two ends planning different sends.
      transfer.attemptClosed = true;
      transfer.chunkBuffers = [];
      transfer.inbox = [];
      transfer.pendingFrames = [];
      transfer.reorder = new Map();
      transfer.reorderBytes = 0;
      return capRanges(coalesceRanges(transfer.verifiedRanges));
    },

    /** Routes one inbound control message; true when consumed. */
    handleControl(message) {
      if (message === null || typeof message !== "object") {
        return false;
      }
      const body = message.body ?? {};
      // A relay-leg failure names its transfer and carries no requestId, so
      // it must be routed before the ack/error block claims every `error`.
      // The server puts that ID in `message` (`error_envelope_anon` has no
      // other field) — the fixture `error.direct_failed` is the shape, and
      // reading only a `transferId` key made this branch dead on the wire.
      if (message.type === "error" && body.code === "DIRECT_FAILED") {
        const named =
          typeof body.message === "string" ? body.message : body.transferId;
        if (typeof named === "string" && transfers.has(named)) {
          failTransfer(named, "DIRECT_FAILED", undefined, false);
          return true;
        }
      }
      if (message.type === "ack" || message.type === "error") {
        if (message.requestId !== undefined) {
          const completing = pendingCompletes.get(message.requestId);
          if (completing !== undefined) {
            pendingCompletes.delete(message.requestId);
            if (message.type === "ack") {
              void stageCompleted(completing.transferId);
            } else {
              // An ERROR here used to fall through to the request table, miss,
              // and return false — so a refused completion was dropped in
              // silence and the transfer could never finish.
              completeRefused(completing, body.code ?? "INTERNAL");
            }
            return true;
          }
        }
        const pending =
          message.requestId === undefined
            ? undefined
            : pendingRequests.get(message.requestId);
        if (pending === undefined) {
          return false;
        }
        pendingRequests.delete(message.requestId);
        if (pending.requestTimer !== undefined) {
          clearTimeout(pending.requestTimer);
        }
        if (message.type === "error") {
          events.onError?.(null, body.code ?? "INTERNAL");
          return true;
        }
        const transferId = body?.result?.transferId ?? body?.transferId;
        if (typeof transferId !== "string") {
          return true;
        }
        pending.transferId = transferId;
        pending.state = "requested";
        transfers.set(transferId, pending);
        // The row appears only now: before the ack there is no transfer ID
        // to name, and before the click there is no request at all.
        events.onStarted?.({
          transferId,
          offerId: pending.offerId,
          sourcePeerId: pending.sourcePeerId,
          totalBytes: Number(pending.totalBytes),
          label: pending.entry.path,
        });
        return true;
      }
      if (message.type === "transfer.relay_ticket") {
        const transfer = transfers.get(body.transferId);
        if (transfer === undefined || typeof body.ticket !== "string") {
          return false;
        }
        transfer.ticket = body.ticket;
        // A ticket naming a DIFFERENT attempt is the fallback: the same
        // transfer continues on the relay with a fresh key and a sequence
        // that restarts at zero, keeping every chunk already verified.
        if (
          typeof body.attemptId === "string" &&
          transfer.attemptId !== null &&
          transfer.attemptId !== body.attemptId
        ) {
          if (
            transfer.state === "complete-pending" ||
            transfer.state === "staged"
          ) {
            return true;
          }
          adoptAttempt(transfer, body.attemptId);
          transfer.state = "requested";
        } else if (transfer.attemptId === null) {
          transfer.attemptId = body.attemptId ?? null;
        }
        if (transfer.state === "requested") {
          transfer.state = "ticketed";
          openRelay(transfer);
          void deriveAttemptKey(transfer);
        }
        return true;
      }
      if (message.type === "transfer.path_commit") {
        const transfer = transfers.get(body.transferId);
        if (transfer === undefined) {
          return false;
        }
        if (body.path === "direct") {
          // The UPGRADE commit: the server has decided this download moves
          // off the relay onto the probe both peers just negotiated. This is
          // the only place a running download changes transport, and it is
          // the same message the fallback uses in the other direction.
          if (transfer.upgrade?.attemptId === body.attemptId) {
            const carriers = transfer.upgrade.carriers;
            try {
              transfer.socket?.close();
            } catch {
              /* already gone */
            }
            transfer.socket = null;
            // `adoptAttempt` resets the key, the sequence and the inbox, and
            // stops the old attempt's idle clock; every frame still in flight
            // on the relay names an attempt that is no longer current and is
            // discarded exactly as a stale frame always was.
            adoptAttempt(transfer, body.attemptId);
            transfer.upgrade = null;
            transfer.reorderWindow = carriers > 1;
            // Same sizing as `beginDirect`: an upgraded attempt runs on the
            // same carriers and needs the same window (B-A040).
            transfer.reorderLimits = reorderBudget(carriers);
            transfer.reorderGapSince = 0;
            transfer.transport = "direct";
            applyCommitPlan(transfer, body.resumeRanges);
            transfer.state = "receiving";
            startIdleClock(transfer);
            void deriveAttemptKey(transfer);
            return true;
          }
          // The channel and the key were built at `direct_start`; the commit
          // only says the source may begin, and must name THIS attempt.
          if (transfer.attemptId !== body.attemptId) {
            return false;
          }
          transfer.transport = "direct";
          applyCommitPlan(transfer, body.resumeRanges);
          if (transfer.state === "direct") {
            transfer.state = "receiving";
          }
          // Only when a chunk of THIS attempt has already been verified: the
          // commit then corrects a badge that named the pre-commit transport.
          // With nothing verified yet the badge stays `in connessione`, which
          // is the rule this call must not break.
          if (transfer.pathAttempt === transfer.attemptId) {
            nameTransport(transfer);
          }
          return true;
        }
        if (body.path !== "relay") {
          return false;
        }
        transfer.attemptId = body.attemptId;
        transfer.transport = "relay";
        applyCommitPlan(transfer, body.resumeRanges);
        if (transfer.state === "ticketed" || transfer.state === "requested") {
          transfer.state = "receiving";
        }
        if (transfer.pathAttempt === transfer.attemptId) {
          nameTransport(transfer);
        }
        return true;
      }
      if (message.type === "transfer.cancelled") {
        const transfer = transfers.get(body.transferId);
        if (transfer === undefined) {
          return false;
        }
        // Remote cancel keeps the verified partial (resume stays possible).
        forget(body.transferId);
        events.onCancelled?.(body.transferId, transfer.offerId);
        return true;
      }
      if (message.type === "transfer.completed") {
        if (!transfers.has(body.transferId)) {
          return false;
        }
        void stageCompleted(body.transferId);
        return true;
      }
      return false;
    },

    /**
     * Stops locally, then sends an idempotent `transfer.cancel`. The abort
     * comes FIRST so no further byte is written or requested even if the
     * control send blocks or the socket is already gone; the partial stays
     * on disk, so the next click resumes it.
     */
    cancelTransfer(transferId) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined) {
        return false;
      }
      forget(transferId);
      sendControl({
        v: 1,
        type: "transfer.cancel",
        requestId: randomRequestId(),
        body: { transferId },
      });
      return true;
    },

    /**
     * Offer IDs with a staged partial on disk. The UI shows them as
     * resumable; nothing here starts a transfer, and only a click can.
     */
    async resumableOffers() {
      const offerIds = new Set();
      let keys = [];
      try {
        keys = (await repository.listKeys()) ?? [];
      } catch {
        return offerIds;
      }
      for (const key of keys) {
        if (
          Array.isArray(key) &&
          key[0] === roomIdHex &&
          typeof key[2] === "string"
        ) {
          offerIds.add(key[2]);
        }
      }
      return offerIds;
    },

    /** Confirms the explicit save and purges staging. The object URL is
     * revoked too, unless `keepUrl` holds it back; the OPFS bytes are
     * deleted too, unless `keepBytes` holds them back. An anchor download
     * fetches both asynchronously after the click, so revoking or deleting
     * with it still in flight cancels the fetch — the anchor path keeps
     * both until room close, withdraw or the next staged file (which purge
     * unconditionally). The file-picker path copies synchronously and
     * purges everything at once. */
    async confirmSaved(
      transferId,
      { keepUrl = false, keepBytes = false } = {},
    ) {
      const staged = stagedFiles.get(transferId);
      if (staged === undefined) {
        return false;
      }
      if (!keepUrl && typeof staged.url === "string") {
        try {
          globalThis.URL?.revokeObjectURL?.(staged.url);
        } catch {
          /* best effort */
        }
      }
      stagedFiles.delete(transferId);
      if (!keepBytes) {
        await purgeTransfer(staged);
      } else {
        try {
          await repository.deleteRecord(staged.recordKey);
        } catch {
          /* best effort */
        }
      }
      return true;
    },

    /** Discards the staged file: revokes the URL and purges staging. */
    async discardStaged(transferId) {
      const staged = stagedFiles.get(transferId);
      if (staged === undefined) {
        return false;
      }
      if (typeof staged.url === "string") {
        try {
          globalThis.URL?.revokeObjectURL?.(staged.url);
        } catch {
          /* best effort */
        }
      }
      stagedFiles.delete(transferId);
      await purgeTransfer(staged);
      return true;
    },

    /** Purges one offer's staging (withdraw): bytes, record and live legs. */
    async purgeOffer(offerId) {
      for (const [transferId, transfer] of transfers) {
        if (transfer.offerId === offerId) {
          forget(transferId);
        }
      }
      for (const [transferId, staged] of stagedFiles) {
        if (staged.offerId === offerId) {
          if (typeof staged.url === "string") {
            try {
              globalThis.URL?.revokeObjectURL?.(staged.url);
            } catch {
              /* best effort */
            }
          }
          stagedFiles.delete(transferId);
          await purgeTransfer(staged);
        }
      }
      await purgeRecords((key) => key[2] === offerId);
    },

    /** Purges everything (room close): legs, URLs, bytes, records. */
    async purgeAll() {
      for (const transferId of [...transfers.keys()]) {
        forget(transferId);
      }
      for (const [transferId, staged] of stagedFiles) {
        if (typeof staged.url === "string") {
          try {
            globalThis.URL?.revokeObjectURL?.(staged.url);
          } catch {
            /* best effort */
          }
        }
        stagedFiles.delete(transferId);
        await purgeTransfer(staged);
      }
      await purgeRecords(() => true);
    },

    startDownload,
    abortTransfer,
  };

  async function purgeTransfer(staged) {
    try {
      await repository.removeDir(staged.dirSegments);
    } catch {
      /* best effort */
    }
    try {
      await repository.deleteRecord(staged.recordKey);
    } catch {
      /* best effort */
    }
  }

  /**
   * Deletes matching resume records AND the staged bytes behind them.
   *
   * The record and the OPFS directory are two halves of one partial: a
   * withdraw or a room close that removed only the record left the chunks
   * on disk for the life of the origin, invisible to every later listing
   * because nothing points at them any more. The directory is derived from
   * the key, which is what makes that impossible to forget — the same four
   * hex segments that name the record name the directory. A raw partial and
   * an archive partial of the same offer have different selection digests,
   * so both are reached, and neither can reach the other's bytes.
   */
  async function purgeRecords(matches) {
    let keys = [];
    try {
      keys = (await repository.listKeys()) ?? [];
    } catch {
      keys = [];
    }
    for (const key of keys) {
      if (!matches(key)) {
        continue;
      }
      try {
        await repository.removeDir(partDirSegments(key[0], key[2], key[3]));
      } catch {
        /* a malformed key has no directory to remove */
      }
      try {
        await repository.deleteRecord(key);
      } catch {
        /* best effort */
      }
    }
  }

  return receiver;
}
