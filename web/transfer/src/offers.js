// Local offer management: intake maps, worker orchestration, control
// wiring. One picker/drop action is one offer; `File` objects live only in
// tab memory keyed by offer/entry. Nothing here reads the network beyond
// the injected `sendControl`; publishing and withdrawal go through it, and
// server replies come back via `handleReply`.

/**
 * @param {object} options
 * @param {() => Worker} options.createWorker worker factory (injected so
 * tests pass a fake; production passes the bundled offer-worker URL)
 * @param {string} options.roomIdHex canonical room ID
 * @param {string} options.roomKeyHex canonical room key (memory only)
 * @param {(message: object) => boolean} options.sendControl control sender
 * @param {object} options.events `{ onProgress(offerId, done, total),
 * onDone(offerId), onError(offerId, message), onWithdrawn(offerId) }`
 */
export function createOfferManager({ createWorker, roomIdHex, roomKeyHex, sendControl, events }) {
  let worker = null;
  let limits = null;
  const records = new Map();
  const pendingReplies = new Map();
  let requestSeq = 0;

  function ensureWorker() {
    if (worker === null) {
      worker = createWorker();
      worker.onmessage = (event) => manager.handleWorkerMessage(event?.data);
    }
    return worker;
  }

  function requestId() {
    requestSeq += 1;
    return `${Date.now().toString(16)}${requestSeq.toString(16).padStart(8, "0")}`
      .padStart(32, "0")
      .slice(-32);
  }

  function labelFor(kind, files) {
    if (kind === "file") {
      return files[0].name || "file";
    }
    if (kind === "folder") {
      const first = files[0].webkitRelativePath || files[0].name || "";
      const top = first.split("/").filter(Boolean)[0];
      return top || `${files.length} files`;
    }
    return `${files.length} files`;
  }

  const manager = {
    /** Advertised server limits from `welcome` (checked before hashing). */
    setServerLimits(next) {
      limits = next ?? null;
    },

    /** Live records (for the view and the test hook). */
    offers() {
      return records;
    },

    /** True when the file still matches its preparation snapshot. */
    isOfferFresh(offerId) {
      const record = records.get(offerId);
      if (!record || !record.observed) {
        return false;
      }
      for (const seen of record.observed) {
        const current = record.files.get(seen.path);
        if (
          !current ||
          current.file.size !== seen.size ||
          Math.floor(current.file.lastModified / 1000) !== seen.mtimeSec
        ) {
          return false;
        }
      }
      return true;
    },

    /** Offer IDs still backed by files and already acked (republish set). */
    republishCandidates() {
      const candidates = [];
      for (const [offerId, record] of records) {
        if (record.status === "live" && record.files.size > 0) {
          candidates.push(offerId);
        }
      }
      return candidates;
    },

    /**
     * Starts one offer from selected files. Returns `{ offerId }` or
     * `{ error }` — cap failures report locally without spawning work or
     * sending anything.
     */
    prepareSelection({ kind, files, label = null }) {
      const list = [...files];
      if (list.length === 0) {
        return { error: "Seleziona almeno un file" };
      }
      if (kind !== "file" && kind !== "files" && kind !== "folder") {
        return { error: "Tipo di offerta sconosciuto" };
      }
      if (kind === "file" && list.length !== 1) {
        return { error: "Un solo file per questa offerta" };
      }
      // Duplicate (case-insensitive, NFC) paths collapse the file map, so
      // they fail here — before any hashing — exactly like the worker and
      // the server would reject them.
      {
        const seen = new Set();
        for (const file of list) {
          const fold = workerRelativePath(file).normalize("NFC").toLowerCase();
          if (seen.has(fold)) {
            return { error: "File duplicati nella selezione" };
          }
          seen.add(fold);
        }
      }
      const caps = {
        maxEntriesPerOffer: limits?.maxEntriesPerOffer ?? 10000,
        maxOfferBytes: limits?.maxOfferBytes ?? 1099511627776,
      };
      if (list.length > caps.maxEntriesPerOffer) {
        return { error: "Troppe voci per una sola offerta" };
      }
      let total = 0n;
      try {
        for (const file of list) {
          total += BigInt(file.size);
        }
      } catch {
        return { error: "File non leggibile" };
      }
      if (total > BigInt(caps.maxOfferBytes)) {
        return { error: "Offerta oltre il limite di dimensione" };
      }
      const bytes = new Uint8Array(16);
      crypto.getRandomValues(bytes);
      const offerId = [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
      const record = {
        offerId,
        kind,
        label: label ?? labelFor(kind, list),
        createdAt: new Date().toISOString(),
        files: new Map(list.map((file) => [fileKey(file), { file }])),
        status: "preparing",
        manifest: null,
        macHex: null,
        observed: null,
        progress: 0,
      };
      records.set(offerId, record);
      try {
        ensureWorker().postMessage({
          type: "prepare",
          offerId,
          kind,
          label: record.label,
          createdAt: record.createdAt,
          files: list.map((file) => ({
            file,
            relativePath: workerRelativePath(file),
          })),
          roomIdHex,
          roomKeyHex,
          limits: caps,
          progress: true,
        });
      } catch (error) {
        records.delete(offerId);
        return { error: String(error?.message ?? error) };
      }
      return { offerId };
    },

    /** Aborts a preparing offer; drops an unacked one silently. */
    abortOffer(offerId) {
      const record = records.get(offerId);
      if (!record) {
        return false;
      }
      if (record.status === "preparing" && worker !== null) {
        try {
          worker.postMessage({ type: "abort", offerId });
        } catch {
          /* worker already gone */
        }
      }
      records.delete(offerId);
      return true;
    },

    /**
     * Withdraws an offer: aborts preparation silently, or sends
     * `offer.withdraw` and drops the maps on the terminal ack.
     */
    withdrawOffer(offerId) {
      const record = records.get(offerId);
      if (!record) {
        return { error: "Offerta sconosciuta" };
      }
      if (record.status === "preparing") {
        this.abortOffer(offerId);
        events.onWithdrawn?.(offerId);
        return {};
      }
      const rid = requestId();
      pendingReplies.set(rid, { offerId, kind: "withdraw" });
      if (!sendControl({ v: 1, type: "offer.withdraw", requestId: rid, body: { offerId } })) {
        pendingReplies.delete(rid);
        return { error: "Non connesso" };
      }
      record.status = "withdrawing";
      return {};
    },

    /** Routes one worker message; returns true when consumed. */
    handleWorkerMessage(message) {
      if (message === null || typeof message !== "object") {
        return false;
      }
      const record = records.get(message.offerId);
      if (message.type === "progress") {
        if (record && typeof message.doneBytes === "number" && typeof message.totalBytes === "number") {
          record.progress = message.totalBytes > 0 ? message.doneBytes / message.totalBytes : 0;
          events.onProgress?.(message.offerId, message.doneBytes, message.totalBytes);
        }
        return true;
      }
      if (!record) {
        return false;
      }
      if (message.type === "done") {
        record.manifest = message.manifest;
        record.macHex = message.macHex;
        record.observed = message.observed;
        record.files = new Map(
          message.observed.map((seen) => {
            const kept = record.files.get(seen.path);
            return [seen.path, kept ?? { file: null }];
          }),
        );
        record.status = "ready";
        const rid = requestId();
        pendingReplies.set(rid, { offerId: record.offerId, kind: "publish" });
        const sent = sendControl({
          v: 1,
          type: "offer.publish",
          requestId: rid,
          body: { offerId: record.offerId, manifest: record.manifest, mac: record.macHex },
        });
        if (!sent) {
          pendingReplies.delete(rid);
          record.status = "error";
          events.onError?.(record.offerId, "Non connesso");
          return true;
        }
        return true;
      }
      if (message.type === "aborted") {
        records.delete(message.offerId);
        return true;
      }
      if (message.type === "error") {
        record.status = "error";
        events.onError?.(message.offerId, message.message ?? "Preparazione fallita");
        return true;
      }
      return false;
    },

    /**
     * Routes one server `ack`/`error` by request ID; returns true when the
     * reply belonged to an offer mutation. Terminal withdraw acks drop the
     * maps; publish results flip the record live.
     */
    handleReply(type, body, requestId) {
      if (requestId === null || requestId === undefined || !pendingReplies.has(requestId)) {
        return false;
      }
      const pending = pendingReplies.get(requestId);
      pendingReplies.delete(requestId);
      const record = records.get(pending.offerId);
      if (type === "ack") {
        if (pending.kind === "publish" && record) {
          record.status = "live";
          events.onDone?.(pending.offerId);
        } else if (pending.kind === "withdraw") {
          records.delete(pending.offerId);
          events.onWithdrawn?.(pending.offerId);
        }
        return true;
      }
      if (type === "error") {
        const code = body?.code ?? "INTERNAL";
        if (pending.kind === "withdraw") {
          // Terminal for this phase (NOT_PARTICIPANT et al.): the maps go,
          // the error surfaces.
          records.delete(pending.offerId);
          events.onWithdrawn?.(pending.offerId);
          events.onError?.(pending.offerId, code);
        } else if (record) {
          record.status = "error";
          events.onError?.(pending.offerId, code);
        }
        return true;
      }
      return false;
    },

    /** Re-sends a live offer after reconnect (fresh request ID). */
    republish(offerId) {
      const record = records.get(offerId);
      if (!record || !record.manifest || !record.macHex) {
        return false;
      }
      const rid = requestId();
      pendingReplies.set(rid, { offerId, kind: "publish" });
      return sendControl({
        v: 1,
        type: "offer.publish",
        requestId: rid,
        body: { offerId, manifest: record.manifest, mac: record.macHex },
      });
    },
  };

  return manager;

  function fileKey(file) {
    const relative =
      file.webkitRelativePath && file.webkitRelativePath !== "" ? file.webkitRelativePath : file.name;
    return relative.startsWith("/") ? relative.slice(1) : relative;
  }

  function workerRelativePath(file) {
    return fileKey(file);
  }
}
