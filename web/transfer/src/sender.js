// Source transfer actor (Phase 3.3, direct-first since 4.2): auto-accepts
// `incoming` for a valid local offer, readies, takes whichever transport the
// server commits — a WebRTC DataChannel handed in by `attachDirect`, or one
// relay leg opened on a ticket — then sends the single file when
// `path_commit` arrives. Plaintext pipeline is
// strictly sequential — one chunk and one frame alive at most — with
// `bufferedAmount` backpressure and an AbortController per transfer.
//
// The wire carries only DATA/FINAL frames (the relay pump rejects anything
// else); CHUNK_DIGEST/ENTRY_DONE/TRANSFER_DONE are internal pipeline stages
// surfaced as events, never new message types. Progress is local-only in
// this phase: the server accepts no `transfer.progress` yet, so the sender
// emits throttled callback events instead of wire traffic.

import {
  attemptKey,
  bytesToHex,
  fileRoot,
  hexToBytes,
  importAttemptAesKey,
  sealFrameWithKey,
  sha256Hex,
} from "./crypto.js";
import { perfEnd, perfStart } from "./perf.js";
import { canonicalize } from "./protocol.js";
import {
  CHUNK_BYTES,
  FRAGMENT_BYTES,
  FRAME_DATA,
  FRAME_FINAL,
  archiveFinalPayload,
  chunkWindow,
  fragmentWindow,
  finalPayload,
  planSend,
} from "./framing.js";
import { createChunkSink, writeArchive } from "./zip-stream.js";
import { rangesToIndexes, verifiedPrefix } from "./storage.js";

/**
 * The chunk indexes an ARCHIVE attempt may skip, from whatever the commit
 * carried.
 *
 * Reduced to the contiguous prefix from chunk zero, exactly as the
 * recipient reduces it (`verifiedPrefix`): an archive chunk has no
 * manifest digest and no independent identity, only a position in a stream
 * the source regenerates, so the recipient stages in arrival order and a
 * hole would put the next arrival one index too high. The reduction is
 * applied on BOTH ends rather than trusted from one: the source is the
 * party that decides what travels, so it must not skip a chunk the
 * recipient will not be sitting at.
 */
function archiveResumeRanges(ranges) {
  const bounded = (Array.isArray(ranges) ? ranges : []).filter(
    (range) =>
      Array.isArray(range) &&
      range.length === 2 &&
      Number.isSafeInteger(range[0]) &&
      Number.isSafeInteger(range[1]) &&
      range[0] >= 0 &&
      range[1] >= range[0],
  );
  return verifiedPrefix(bounded);
}

/** Local cap on concurrent outgoing transfers (plan-fixed). */
export const SENDER_MAX_CONCURRENT = 8;
/** Above this queued bytes the pipeline stops reading new slices. */
export const SEND_HIGH_WATER = 4 * 1024 * 1024;
/** Reading resumes once queued bytes drop below this. */
export const SEND_LOW_WATER = 1 * 1024 * 1024;
/** Progress events: at most one per 500 ms … */
export const PROGRESS_MIN_MS = 500;
/** … or per MiB sent, whichever fires first. */
export const PROGRESS_MIN_BYTES = 1024 * 1024;
/** Backpressure poll step while the socket drains. */
export const DRAIN_POLL_MS = 30;

function randomRequestId() {
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}

function sleep(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) {
      reject(new DOMException("aborted", "AbortError"));
      return;
    }
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(new DOMException("aborted", "AbortError"));
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/**
 * @param {object} options
 * @param {object} options.offers offer manager (`offers()`, `withdrawOffer()`)
 * @param {(message: object) => boolean} options.sendControl control sender
 * @param {(url: string, protocol: string) => WebSocket} options.createSocket
 * relay socket factory (injected for tests)
 * @param {string} options.relayBase `ws(s)://host` prefix for relay URLs
 * @param {string} options.roomIdHex canonical room ID
 * @param {string} options.roomKeyHex 64-hex room key (memory only)
 * @param {() => string|null} options.getSelfPeerId own peer ID for the attach body
 * @param {object} options.events `{ onProgress(info), onChunk(transferId,
 * chunkIndex), onEntryDone(transferId), onDone(transferId, info),
 * onError(transferId, code), onSourceChanged(offerId) }` (all optional)
 */
export function createSender({
  offers,
  sendControl,
  createSocket,
  relayBase,
  roomIdHex,
  roomKeyHex,
  getSelfPeerId,
  events = {},
}) {
  /** transferId → live transfer state (deleted at every terminal step). */
  const transfers = new Map();

  function activeCount() {
    let count = 0;
    for (const transfer of transfers.values()) {
      if (
        transfer.state !== "done" &&
        transfer.state !== "error" &&
        transfer.state !== "aborted"
      ) {
        count += 1;
      }
    }
    return count;
  }

  function sendReady(transferId, attemptId, selectionDigest) {
    return sendControl({
      v: 1,
      type: "transfer.source_ready",
      requestId: randomRequestId(),
      body: { transferId, attemptId, selectionDigest },
    });
  }

  function sendReject(transferId, code) {
    return sendControl({
      v: 1,
      type: "transfer.reject",
      requestId: randomRequestId(),
      body: { transferId, code },
    });
  }

  /**
   * The send half of one transport, in the shape the pipeline uses. The
   * relay's WebSocket has no drain event, so it polls; the DataChannel's
   * sink (built in `webrtc.js`) awaits `bufferedamountlow` instead. Nothing
   * else about the pipeline differs between the two paths.
   */
  function relaySink(socket) {
    return {
      fragmentBytes: FRAGMENT_BYTES,
      highWater: SEND_HIGH_WATER,
      get bufferedAmount() {
        return socket.bufferedAmount ?? 0;
      },
      send(bytes) {
        socket.send(bytes);
      },
      async waitLow(signal) {
        while ((socket.bufferedAmount ?? 0) > SEND_LOW_WATER) {
          await sleep(DRAIN_POLL_MS, signal);
        }
      },
      close() {
        try {
          socket.close();
        } catch {
          /* already gone */
        }
      },
    };
  }

  function closeTransport(transfer) {
    try {
      transfer.sink?.close();
    } catch {
      /* already gone */
    }
    try {
      transfer.socket?.close();
    } catch {
      /* already gone */
    }
    transfer.sink = null;
    transfer.socket = null;
  }

  function forget(transferId) {
    const transfer = transfers.get(transferId);
    if (transfer !== undefined) {
      transfer.abort.abort();
      transfer.attemptAbort.abort();
      closeTransport(transfer);
      transfer.aesKey = null;
      transfers.delete(transferId);
    }
  }

  /** Public abort: user cancel (3.5) and room-close purge ride this. */
  function abortTransfer(transferId) {
    const transfer = transfers.get(transferId);
    if (transfer === undefined) {
      return false;
    }
    forget(transferId);
    return true;
  }

  async function waitForLowWater(transfer) {
    const sink = transfer.sink;
    if (
      sink === null ||
      sink === undefined ||
      sink.bufferedAmount <= sink.highWater
    ) {
      return;
    }
    await sink.waitLow(transfer.attemptAbort.signal);
  }

  function emitProgress(transfer, force = false) {
    const now = Date.now();
    if (
      !force &&
      now - transfer.lastProgressAt < PROGRESS_MIN_MS &&
      transfer.sentBytes - transfer.lastProgressBytes < PROGRESS_MIN_BYTES
    ) {
      return;
    }
    transfer.lastProgressAt = now;
    transfer.lastProgressBytes = transfer.sentBytes;
    events.onProgress?.({
      transferId: transfer.transferId,
      sentBytes: transfer.sentBytes,
      // An archive's total is the packed bytes plus headers, which is only
      // known once it has been written: until then the estimate is a lower
      // bound, and a lower bound that has been passed is not a total.
      totalBytes: transfer.archive
        ? Math.max(transfer.totalBytes, transfer.sentBytes)
        : transfer.totalBytes,
    });
  }

  /**
   * Streams the whole offer as ONE deterministic archive and returns its
   * sealed FINAL frame.
   *
   * Nothing is buffered beyond a single logical chunk: `createChunkSink`
   * does not accept the next megabyte until this consumer has sealed and
   * queued the previous one, so the archive's memory cost is independent of
   * its size and the backpressure is the transport's, exactly as it is for
   * a raw file.
   *
   * The FINAL tuple is the archive's whole contract — its length, its chunk
   * count and the rolling root over its leaves are in no manifest, because
   * the archive does not exist until it is generated. It is sealed under the
   * attempt key, which is what makes the recipient's check a check against
   * the SOURCE and not against whatever produced the bytes.
   *
   * A RESUME regenerates the archive from byte zero and hashes every chunk
   * — `skip` only decides what goes on the WIRE. Both halves are load
   * bearing: the root covers every leaf, including the ones the recipient
   * already holds, so a source whose files changed under a verified partial
   * fails the root instead of quietly producing a corrupt archive; and
   * regenerating is what makes the skipped chunks meaningful at all, since
   * `ZIP_WRITER_OPTIONS` fixes every field a ZIP writer would otherwise
   * vary and a second pass over the same manifest and files is
   * byte-identical to the first.
   */
  async function sendArchive(transfer, signal, skip = new Set()) {
    const abortIfStale = () => {
      if (signal.aborted) {
        throw new DOMException("aborted", "AbortError");
      }
    };
    let seq = 0;
    const leaves = [];
    const archive = createChunkSink({
      onChunk: async (chunk, index) => {
        const drainAt = perfStart();
        await waitForLowWater(transfer);
        perfEnd("src.drain", drainAt);
        abortIfStale();
        const hashAt = perfStart();
        leaves.push(hexToBytes(await sha256Hex(chunk)));
        perfEnd("src.hash", hashAt, chunk.length);
        abortIfStale();
        events.onChunk?.(transfer.transferId, index);
        if (skip.has(index)) {
          // The recipient holds this chunk and told the server so. Its
          // leaf is already in the root above; its bytes stay home. It
          // still counts as progress — the transfer is as far along as the
          // recipient's disk says, not as far as this attempt's wire.
          transfer.baseBytes += chunk.length;
          transfer.sentBytes = transfer.baseBytes + transfer.attemptBytes;
          emitProgress(transfer);
          return;
        }
        for (const fragment of fragmentWindow(
          0,
          chunk.length,
          transfer.sink.fragmentBytes,
        )) {
          const plaintext = chunk.subarray(
            fragment.offset,
            fragment.offset + fragment.length,
          );
          const sealAt = perfStart();
          const sealed = await sealFrameWithKey(
            transfer.aesKey,
            seq,
            FRAME_DATA,
            plaintext,
          );
          perfEnd("src.seal", sealAt, fragment.length);
          abortIfStale();
          const sendAt = perfStart();
          transfer.sink.send(sealed);
          perfEnd("src.send", sendAt, fragment.length);
          seq += 1;
          transfer.attemptBytes += fragment.length;
          transfer.sentBytes = transfer.baseBytes + transfer.attemptBytes;
          emitProgress(transfer);
        }
      },
    });
    await writeArchive({
      entries: transfer.entries,
      fileFor: transfer.fileFor,
      writable: archive.writable,
      signal,
    });
    abortIfStale();
    events.onEntryDone?.(transfer.transferId);
    return sealFrameWithKey(
      transfer.aesKey,
      seq,
      FRAME_FINAL,
      archiveFinalPayload(
        archive.bytes,
        archive.chunks,
        await fileRoot(leaves.length, leaves),
      ),
    );
  }

  /**
   * Runs the send pipeline for one committed transfer. Resume ranges ride
   * the commit body as a tolerated extension (see above); the control path
   * forwards whatever the commit carried.
   */
  async function beginSend(transferId, resumeRanges = []) {
    const transfer = transfers.get(transferId);
    if (transfer === undefined || transfer.state !== "committed") {
      return false;
    }
    transfer.state = "sending";
    // This attempt has not written its end-of-stream marker yet. It is
    // per-ATTEMPT, so a relay leg that follows a dead direct one starts
    // honest again.
    transfer.finalSent = false;
    transfer.attemptAbort = new AbortController();
    const { signal } = transfer.attemptAbort;
    try {
      const keyBytes = await attemptKey(
        hexToBytes(roomKeyHex),
        hexToBytes(transfer.transferId),
        hexToBytes(transfer.attemptId),
      );
      if (signal.aborted) {
        return false;
      }
      transfer.aesKey = await importAttemptAesKey(keyBytes);
      keyBytes.fill(0);
      // An archive has no chunk count before it exists, so it has no plan:
      // `planSend` cannot bound ranges it has no total for. Its skip set is
      // consulted per generated chunk instead, inside `sendArchive`.
      const archiveSkip = transfer.archive
        ? new Set(rangesToIndexes(archiveResumeRanges(resumeRanges)))
        : new Set();
      const { send, rehashOnly } = planSend(
        transfer.chunkCount,
        transfer.archive ? [] : resumeRanges,
      );
      const sendSet = new Set(send);
      // Progress must not walk backwards across a fallback: the chunks the
      // recipient already holds count as sent, and only the rest travels.
      let baseBytes = 0;
      for (const index of rehashOnly) {
        baseBytes += chunkWindow(transfer.totalBytes, index).length;
      }
      transfer.baseBytes = baseBytes;
      transfer.attemptBytes = 0;
      transfer.sentBytes = baseBytes;
      transfer.lastProgressBytes = baseBytes;
      let seq = 0;
      // Everything between this peer learning of the request and its first
      // read: ready, ticket, relay attach, commit and the attempt key.
      perfEnd("src.setup", transfer.incomingAt);
      const wallAt = perfStart();
      let final = null;
      if (transfer.archive) {
        final = await sendArchive(transfer, signal, archiveSkip);
      }
      for (
        let index = 0;
        !transfer.archive && index < transfer.chunkCount;
        index++
      ) {
        const drainAt = perfStart();
        await waitForLowWater(transfer);
        perfEnd("src.drain", drainAt);
        if (signal.aborted) {
          return false;
        }
        const { offset, length } = chunkWindow(transfer.totalBytes, index);
        const readAt = perfStart();
        const bytes = new Uint8Array(
          await transfer.file.slice(offset, offset + length).arrayBuffer(),
        );
        perfEnd("src.read", readAt, length);
        if (signal.aborted) {
          return false;
        }
        // Every chunk is rehashed — skipped resume ranges too — before
        // anything is sent or discarded.
        const hashAt = perfStart();
        const digest = await sha256Hex(bytes);
        perfEnd("src.hash", hashAt, length);
        if (signal.aborted) {
          return false;
        }
        if (digest !== transfer.chunkDigests[index]) {
          throw { code: "SOURCE_CHANGED" };
        }
        events.onChunk?.(transferId, index);
        if (!sendSet.has(index)) {
          continue;
        }
        for (const fragment of fragmentWindow(
          offset,
          length,
          transfer.sink.fragmentBytes,
        )) {
          const plaintext = bytes.subarray(
            fragment.offset - offset,
            fragment.offset - offset + fragment.length,
          );
          const sealAt = perfStart();
          const sealed = await sealFrameWithKey(
            transfer.aesKey,
            seq,
            FRAME_DATA,
            plaintext,
          );
          perfEnd("src.seal", sealAt, fragment.length);
          if (signal.aborted) {
            return false;
          }
          const sendAt = perfStart();
          transfer.sink.send(sealed);
          perfEnd("src.send", sendAt, fragment.length);
          seq += 1;
          transfer.attemptBytes += fragment.length;
          transfer.sentBytes = transfer.baseBytes + transfer.attemptBytes;
          emitProgress(transfer);
        }
      }
      if (!transfer.archive) {
        events.onEntryDone?.(transferId);
        // FINAL counts the plaintext that travelled on THIS attempt, which
        // on a resumed transfer excludes every chunk the recipient already
        // holds. Sending the whole entry size instead would fail the
        // recipient's FINAL check on exactly the resumes this phase exists
        // to support.
        final = await sealFrameWithKey(
          transfer.aesKey,
          seq,
          FRAME_FINAL,
          finalPayload(transfer.attemptBytes),
        );
      }
      if (signal.aborted) {
        return false;
      }
      transfer.sink.send(final);
      // FINAL is written: from here the source has nothing left to say, and
      // a close is no longer evidence of failure (see `socket.onclose`).
      transfer.finalSent = true;
      perfEnd("src.wall", wallAt, transfer.attemptBytes);
      // Wait until the transport has actually WRITTEN everything the loop
      // handed it. `send` returns as soon as the bytes are queued, and on
      // the relay leg the close below is the end-of-stream signal — a close
      // issued with megabytes still queued is only as safe as the engine's
      // own flush-on-close. MEASURED on WebKit (4.4, the three-peer
      // scenario): the source had written all 8 388 613 bytes, the server
      // had forwarded 6 398 144 of them, and the leg ended — the server saw
      // `SourceGone` and failed a transfer whose every byte was already
      // written, on both peers, with `DIRECT_FAILED`. It is reachable
      // whenever the recipient is slower than the source, which the
      // server's own `--web-transfer-relay-rate` guarantees. The same wait
      // is the harness's `src.flush` stage (3.10): `src.wall` short with
      // `src.flush` long means the queue, not the pipeline, sets the rate.
      const flushAt = perfStart();
      while ((transfer.sink?.bufferedAmount ?? 0) > 0) {
        await sleep(1, signal);
      }
      perfEnd("src.flush", flushAt, transfer.attemptBytes);
      transfer.sentBytes += 0;
      emitProgress(transfer, true);
      transfer.state = "done-pending";
      transfer.aesKey = null;
      // NOTHING is closed here, on either transport. The FINAL frame is the
      // end-of-stream signal on both, and the party that must act on it is
      // the one reading: the server stops the relay pump on it, the
      // recipient stops its DataChannel reader on it. Closing the relay leg
      // here used to be that signal, and it made the end of a transfer a
      // property of the ENGINE's flush-on-close — WebKit ended a leg at
      // 7 087 168 bytes of 8 388 613 already written and the server failed
      // the whole transfer as `SourceGone`. The transport dies with the
      // transfer's terminal control message instead.
      return true;
    } catch (error) {
      if (transfer.abort.signal.aborted) {
        forget(transferId);
        return false;
      }
      if (signal.aborted || error?.name === "AbortError") {
        // Only THIS attempt was abandoned (the direct channel died): the
        // transfer stays tracked and waits for the relay attempt.
        return false;
      }
      const code = error?.code ?? "FAILED";
      if (code === "SOURCE_CHANGED") {
        events.onSourceChanged?.(transfer.offerId);
      }
      events.onError?.(transferId, code);
      forget(transferId);
      return false;
    }
  }

  /**
   * Finds what the request selected — the single servable file entry for
   * `raw`, or the whole offer for `zip` — or rejects the incoming.
   */
  async function startIncoming(message) {
    const incomingAt = perfStart();
    const {
      transferId,
      offerId,
      attemptId,
      recipientPeerId = null,
      mode = "raw",
    } = message;
    if (transfers.has(transferId)) {
      // Duplicate/redelivered incoming for a tracked transfer: the ready
      // already left (or the pipeline runs); never double-ready.
      return;
    }
    if (activeCount() >= SENDER_MAX_CONCURRENT) {
      sendReject(transferId, "BUSY");
      return;
    }
    const archive = mode === "zip";
    const record = offers.offers().get(offerId);
    const manifest = record?.manifest;
    const allEntries = Array.isArray(manifest?.entries) ? manifest.entries : [];
    const fileEntries = allEntries.filter(
      (entry) => Array.isArray(entry.chunks) && entry.chunks.length > 0,
    );
    // `ready` (published, ack in flight) serves too: the server only sends
    // `incoming` for offers it knows, so a racing ack never over-serves.
    const servable = archive ? allEntries.length > 0 : fileEntries.length === 1;
    if (
      (record?.status !== "live" && record?.status !== "ready") ||
      !servable
    ) {
      sendReject(transferId, "OFFER_NOT_FOUND");
      return;
    }
    // A RAW transfer reads one file and must know now that it is unchanged;
    // an archive reads every file in the offer and checks each of them as
    // `writeArchive` reaches it, which is also the only moment the check
    // would still be true. Both answer `SOURCE_CHANGED`.
    const entry = archive ? null : fileEntries[0];
    const fileFor = (path) => record.files.get(path)?.file ?? null;
    if (!archive) {
      const file = fileFor(entry.path);
      const size = Number(entry.size);
      const mtimeSec = Number(entry.mtime);
      if (
        file === null ||
        !(
          file.size === size &&
          Math.floor(file.lastModified / 1000) === mtimeSec
        )
      ) {
        sendReject(transferId, "SOURCE_CHANGED");
        events.onSourceChanged?.(offerId);
        return;
      }
    }
    // The server rejects an unsorted `entryIds`, so the list the recipient
    // sent — and therefore the list this digest must cover — is sorted
    // LEXICOGRAPHICALLY, which for eleven entries puts "10" before "2".
    const entryIds = archive
      ? allEntries.map((each) => String(each.id)).sort()
      : [entry.id];
    const selectionDigest = await sha256Hex(
      new TextEncoder().encode(
        canonicalize({
          entryIds,
          manifestMac: record.macHex,
          mode,
          offerId,
        }),
      ),
    );
    // Store-only, so the archive is never smaller than the bytes it packs:
    // the sum is a lower bound and the right thing to show until FINAL.
    const size = archive
      ? allEntries.reduce((total, each) => total + Number(each.size ?? 0), 0)
      : Number(entry.size);
    const file = archive ? null : fileFor(entry.path);
    const transfer = {
      incomingAt,
      transferId,
      attemptId,
      offerId,
      archive,
      entryId: archive ? null : entry.id,
      // Archive only: the manifest entries in manifest order (the order the
      // archive writes them in) and the lookup that resolves each to a File.
      entries: archive ? allEntries : [],
      fileFor,
      file,
      totalBytes: size,
      chunkCount: archive ? 0 : Number(entry.chunkCount),
      chunkDigests: archive ? [] : entry.chunks,
      aesKey: null,
      socket: null,
      sink: null,
      direct: false,
      pendingResumeRanges: [],
      /** A staged direct channel awaiting the server's upgrade commit. */
      upgrade: null,
      ticket: null,
      abort: new AbortController(),
      // A SECOND controller, scoped to one attempt: abandoning the direct
      // attempt must stop its pipeline without cancelling the transfer,
      // which continues on the relay attempt the server mints.
      attemptAbort: new AbortController(),
      seq: 0,
      baseBytes: 0,
      attemptBytes: 0,
      sentBytes: 0,
      verifiedBytes: 0,
      lastProgressAt: 0,
      lastProgressBytes: 0,
      state: "waiting-commit",
    };
    transfers.set(transferId, transfer);
    events.onStarted?.({
      transferId,
      offerId,
      recipientPeerId,
      totalBytes: size,
      label: archive ? (manifest?.label ?? "") : entry.path,
    });
    if (!sendReady(transferId, attemptId, selectionDigest)) {
      events.onError?.(transferId, "OFFLINE");
      forget(transferId);
    }
  }

  function openRelay(transfer) {
    const socket = createSocket(
      `${relayBase}/transfer/ws/relay/${roomIdHex}/${transfer.transferId}`,
      "bore-transfer-v1",
    );
    transfer.socket = socket;
    transfer.sink = relaySink(socket);
    transfer.direct = false;
    socket.onopen = () => {
      if (transfer.ticket === null) {
        return;
      }
      const selfPeerId = getSelfPeerId();
      if (typeof selfPeerId !== "string") {
        forget(transfer.transferId);
        return;
      }
      try {
        socket.send(
          JSON.stringify({
            v: 1,
            peerId: selfPeerId,
            transferId: transfer.transferId,
            attemptId: transfer.attemptId,
            role: "source",
            ticket: transfer.ticket,
          }),
        );
      } catch {
        forget(transfer.transferId);
      }
    };
    socket.onmessage = () => {
      // The server never sends on relay legs; anything inbound aborts.
      events.onError?.(transfer.transferId, "FAILED");
      forget(transfer.transferId);
    };
    socket.onclose = () => {
      const known = transfers.get(transfer.transferId);
      if (known === undefined) {
        return;
      }
      // Anything before FINAL is a failure the server already reported (or
      // will), and the record dies with it. `finalSent` and not yet
      // `done-pending` is the window between the FINAL write and the end of
      // the flush wait, and it is REACHED: the recipient verifies and the
      // server tears the leg down while the source is still draining its
      // own queue. MEASURED on chromium in `T-WEB-DIRECT-FALLBACK` — the
      // recipient read `relay 100% · Verificato` and the source, for the
      // same transfer, `relay 100% · Non riuscito`. The source cannot tell
      // a truncated queue from a completed one anyway (it is the writer),
      // so once FINAL is out the verdict belongs to the server's own
      // terminal message, exactly as it does after `done-pending`.
      if (known.state !== "done-pending" && !known.finalSent) {
        events.onError?.(transfer.transferId, "FAILED");
        forget(transfer.transferId);
        return;
      }
      // Orderly close after FINAL: the LEG is over, the TRANSFER is not.
      // The recipient still has to verify what it wrote and report it, and
      // that report is the source's ONLY way to learn which path carried
      // the bytes — it cannot read a relay leg apart from a DataChannel
      // from its own socket (4.3's single-authority path badge). Forgetting
      // here dropped the report, so on the relay the source's badge stayed
      // "in connessione" for a transfer that had already completed. The
      // record now dies at `transfer.completed`/`transfer.cancelled`,
      // exactly as it does on the direct path, and only the transport is
      // released here.
      closeTransport(known);
    };
    socket.onerror = () => {
      // The close event follows and carries the decision.
    };
  }

  const sender = {
    /** Live transfer snapshot for tests (and later the transfer rows). */
    transfers() {
      return transfers;
    },

    /** Routes one inbound control message; true when consumed. */
    handleControl(message) {
      if (message === null || typeof message !== "object") {
        return false;
      }
      const body = message.body ?? {};
      if (message.type === "transfer.incoming") {
        if (
          typeof body.transferId !== "string" ||
          typeof body.offerId !== "string" ||
          typeof body.attemptId !== "string"
        ) {
          return false;
        }
        void startIncoming({
          transferId: body.transferId,
          offerId: body.offerId,
          attemptId: body.attemptId,
          // The requester: the only party this tab sends to on this leg.
          recipientPeerId:
            typeof body.fromPeerId === "string" ? body.fromPeerId : null,
          // Which SELECTION was requested. The source recomputes the
          // selection digest itself, and the digest covers the mode, so a
          // mode this end does not know about would fail there rather than
          // silently serve the other selection. Absent on an older server:
          // `raw` is what that server could only have meant.
          mode: body.mode === "zip" ? "zip" : "raw",
        });
        return true;
      }
      if (message.type === "transfer.relay_ticket") {
        const transfer = transfers.get(body.transferId);
        if (
          transfer === undefined ||
          typeof body.ticket !== "string" ||
          typeof body.attemptId !== "string"
        ) {
          return false;
        }
        // A ticket naming a DIFFERENT attempt is the server saying the
        // previous attempt was abandoned and this same transfer continues on
        // the relay with a fresh key and nonce sequence. The SERVER is the
        // authority on attempts, so this is adopted whatever this end was
        // doing — including mid-send.
        //
        // It used to be refused unless the source had not written a byte yet,
        // and that was wrong in the one case it matters. With carriers the
        // recipient's failure report regularly reaches the server while this
        // end is still pushing into a dead channel group: the server mints
        // the relay attempt and tickets it, this end reads `committed`,
        // refuses the ticket, never attaches its relay leg — and the
        // recipient's leg, already attached, waits alone until the 30 s
        // pairing timeout and the transfer dies with the file half
        // delivered. MEASURED: `T-WEB-DIRECT-FALLBACK` fails with four
        // carriers and passes with one, on the same 8 MiB and the same kill.
        //
        // Aborting in flight is the same move the upgrade commit makes in
        // the other direction: the send unwinds through `beginSend`'s
        // AbortError arm and leaves the record in place.
        if (transfer.attemptId !== body.attemptId) {
          transfer.attemptAbort.abort();
          transfer.attemptId = body.attemptId;
          // The previous attempt is over: its channel is closed by the actor
          // that owns it, and the relay leg below installs the new sink. A
          // commit that was parked waiting for that channel is void — the
          // relay commit for the NEW attempt is what starts the pipeline.
          transfer.sink = null;
          transfer.direct = false;
          transfer.pendingResumeRanges = [];
          transfer.state = "waiting-commit";
        }
        transfer.ticket = body.ticket;
        if (transfer.state === "waiting-commit") {
          // The leg opens at ticket time: pairing needs our attach, while
          // the payload pipeline waits for path_commit (no file reads yet).
          openRelay(transfer);
        } else if (transfer.state === "committed-waiting-ticket") {
          // Commit outran the ticket: resume now.
          transfer.state = "waiting-commit";
          openRelay(transfer);
        }
        return true;
      }
      if (message.type === "transfer.path_commit") {
        const transfer = transfers.get(body.transferId);
        if (transfer === undefined) {
          return false;
        }
        // The UPGRADE commit: the server has decided this transfer moves off
        // the relay and onto the probe both peers just negotiated. It is the
        // ONLY thing that may switch a sending transfer's transport, and it
        // is the same message the fallback uses in the other direction.
        if (
          body.path === "direct" &&
          transfer.upgrade !== null &&
          transfer.upgrade.attemptId === body.attemptId
        ) {
          const staged = transfer.upgrade;
          transfer.upgrade = null;
          // Stop the RELAY pipeline the way a fallback stops a direct one: an
          // in-flight send unwinds through the AbortError arm of `beginSend`
          // and leaves the record in place.
          transfer.attemptAbort.abort();
          transfer.attemptId = body.attemptId;
          transfer.sink = staged.sink;
          transfer.direct = true;
          transfer.socket = null;
          transfer.ticket = null;
          transfer.state = "committed";
          // A fresh key and a fresh nonce sequence, from the new attempt ID —
          // `beginSend` derives both, exactly as it does for a relay attempt
          // minted by a fallback.
          void beginSend(transfer.transferId, body.resumeRanges ?? []);
          return true;
        }
        if (transfer.attemptId !== body.attemptId) {
          return false;
        }
        if (body.path === "direct") {
          // Only a commit whose transport is already attached may start the
          // pipeline: the channel is what `beginSend` writes into, and a
          // commit that outran `attachDirect` parks exactly as the relay's
          // does when it outruns its ticket.
          if (transfer.state !== "waiting-commit") {
            return true;
          }
          if (transfer.sink === null) {
            transfer.state = "committed-waiting-direct";
            transfer.pendingResumeRanges = body.resumeRanges ?? [];
            return true;
          }
          transfer.state = "committed";
          void beginSend(transfer.transferId, body.resumeRanges ?? []);
          return true;
        }
        if (body.path !== "relay") {
          return false;
        }
        if (transfer.state === "waiting-commit") {
          if (transfer.ticket === null) {
            // Commit outran the ticket: park until it lands.
            transfer.state = "committed-waiting-ticket";
          } else {
            transfer.state = "committed";
            // `resumeRanges` is a tolerated extension (the server never sends
            // it in this phase; tests drive the skip path through here until
            // the recipient resume flow lands in 3.5).
            void beginSend(transfer.transferId, body.resumeRanges ?? []);
          }
        } else if (transfer.state === "committed-waiting-ticket") {
          // Already parked; the ticket arm resumes.
        }
        return true;
      }
      if (message.type === "transfer.progress") {
        // The recipient's VERIFIED bytes, forwarded by the server with the
        // path the server itself committed. This is the source's only way to
        // learn either one: it cannot read the path off its own socket, and
        // its own `sentBytes` says what left, not what was checked.
        const transfer = transfers.get(body.transferId);
        if (transfer === undefined || transfer.attemptId !== body.attemptId) {
          return false;
        }
        if (body.path === "direct" || body.path === "relay") {
          events.onPath?.(transfer.transferId, body.path);
        }
        const verified = Number(body.receivedBytes);
        if (Number.isSafeInteger(verified) && verified >= 0) {
          transfer.verifiedBytes = verified;
          events.onProgress?.({
            transferId: transfer.transferId,
            sentBytes: Math.max(verified, transfer.sentBytes),
            totalBytes: transfer.totalBytes,
          });
        }
        return true;
      }
      if (
        message.type === "transfer.cancelled" ||
        message.type === "transfer.completed"
      ) {
        const transfer = transfers.get(body.transferId);
        if (transfer === undefined) {
          return false;
        }
        if (message.type === "transfer.completed") {
          events.onDone?.(transfer.transferId, { bytes: transfer.sentBytes });
        } else {
          events.onCancelled?.(transfer.transferId, transfer.offerId);
        }
        forget(body.transferId);
        return true;
      }
      if (
        message.type === "error" &&
        body.code === "DIRECT_FAILED" &&
        typeof body.message === "string" &&
        transfers.has(body.message)
      ) {
        events.onError?.(body.message, "DIRECT_FAILED");
        forget(body.message);
        return true;
      }
      return false;
    },

    /**
     * Hands this transfer the DataChannel's send half. Refused for an
     * attempt that is not the current one — a channel that belongs to an
     * abandoned attempt must never carry a byte of the new one.
     */
    attachDirect(transferId, attemptId, sink) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || transfer.attemptId !== attemptId) {
        return false;
      }
      if (
        transfer.state !== "waiting-commit" &&
        transfer.state !== "committed-waiting-direct"
      ) {
        return false;
      }
      transfer.sink = sink;
      transfer.direct = true;
      if (transfer.state === "committed-waiting-direct") {
        transfer.state = "committed";
        void beginSend(transferId, transfer.pendingResumeRanges ?? []);
      }
      return true;
    },

    /**
     * The direct transport for this transfer is gone. The pipeline had not
     * started (a commit needs an attached channel), so the transfer simply
     * waits for the relay attempt the server mints.
     */
    detachDirect(transferId, attemptId) {
      const transfer = transfers.get(transferId);
      if (
        transfer === undefined ||
        transfer.attemptId !== attemptId ||
        !transfer.direct
      ) {
        return false;
      }
      if (transfer.state === "done-pending") {
        // Every byte is out and the server's `transfer.completed` is the only
        // thing left: the channel closing now is the END, not a failure.
        transfer.direct = false;
        return false;
      }
      // Stops this attempt's pipeline, never the transfer: a send already in
      // flight unwinds through the AbortError arm of `beginSend`, which
      // leaves the record in place for the relay attempt.
      transfer.attemptAbort.abort();
      transfer.sink = null;
      transfer.direct = false;
      if (transfer.state === "sending" || transfer.state === "committed") {
        transfer.state = "waiting-commit";
      }
      return true;
    },

    /**
     * A direct channel for an UPGRADE: the transfer is sending on the relay
     * right now and must keep sending until the server commits the switch.
     *
     * So the sink is only STAGED. Nothing about the live attempt moves here —
     * not the attempt ID, not the key, not the pipeline — because a probe
     * that fails must leave the relay exactly as it found it, and a probe
     * that succeeds is switched by the commit, which is the one message both
     * peers receive.
     */
    attachUpgrade(transferId, attemptId, sink) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || typeof attemptId !== "string") {
        return false;
      }
      if (transfer.attemptId === attemptId) {
        return false;
      }
      transfer.upgrade = { attemptId, sink };
      return true;
    },

    /** The probe is over; the relay was never touched, so nothing unwinds. */
    detachUpgrade(transferId, attemptId) {
      const transfer = transfers.get(transferId);
      if (transfer === undefined || transfer.upgrade?.attemptId !== attemptId) {
        return false;
      }
      transfer.upgrade = null;
      return true;
    },

    beginSend,
    abortTransfer,
  };

  return sender;
}
