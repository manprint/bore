// Deterministic store-mode ZIP64, produced STRAIGHT into the encrypted
// stream (plan 001, sub-phase 5.2).
//
// The archive is never a Blob, an ArrayBuffer or a file: `ZipWriter` writes
// into a `WritableStream` that holds at most ONE logical 1 MiB chunk, hands
// that chunk to the frame sender and does not accept more until the sender
// has taken it. Backpressure is therefore the transport's, exactly as it is
// for a raw file, and the memory cost of a 40 GiB archive is the same as
// that of a 4 MiB one.
//
// Determinism is the other half: the same offer and the same files must
// produce the same bytes on every supported engine, because a resumed
// download regenerates the archive from byte zero and compares what it
// already verified. Everything that could vary is therefore pinned -- no
// compression, no worker, no buffered write, no extended timestamps, no
// comment, no variable extra field -- and every timestamp comes from the
// MANIFEST, which is signed, rather than from the file on disk, which is
// not.
import { ZipWriter } from "@zip.js/zip.js/lib/zip-core-writer.js";
import { CHUNK_BYTES } from "./framing.js";

/**
 * The options that make the output deterministic. Frozen because they are a
 * contract with the recipient (and with a resumed transfer), not a default:
 * changing one of them changes every archive this app has ever produced.
 */
export const ZIP_WRITER_OPTIONS = Object.freeze({
  // Store: the payload is already sealed per chunk and a compressor would
  // make the output depend on its own version.
  level: 0,
  // Always, not only past 4 GiB: a format that changes shape at a size
  // boundary is a format with two behaviours to test.
  zip64: true,
  // The page owns its threads; a worker pool would also make the output
  // depend on scheduling.
  useWebWorkers: false,
  useCompressionStream: false,
  // Write each entry straight through, in manifest order.
  bufferedWrite: false,
  keepOrder: true,
  // A DATA DESCRIPTOR after each entry, and this one is load-bearing rather
  // than cosmetic. zip.js needs the CRC32 to complete a local header, and
  // the only way to have it before the payload is to read the payload
  // first: with `dataDescriptor: false` its writer takes the BUFFERED
  // branch (`zip-writer.js`, `(!dataDescriptor && !emptyEntry)`) through a
  // `TransformStream` whose `highWaterMark` is INFINITY, so the whole entry
  // is held in memory and this sink's backpressure never reaches the
  // reader. MEASURED on a 64 MiB entry with a sink taking 20 ms per chunk:
  // the reader ran 63.0 MiB ahead at 174 MiB RSS with the descriptor off,
  // and 0.8 MiB ahead at 89 MiB RSS with it on — under one logical chunk,
  // which is the promise this module exists to keep. The same change is
  // what makes an abort an abort: abandoning the attempt on a 5 GiB source
  // read 458 KiB in 8 ms instead of the whole 5 GiB in 7 s, because a
  // buffered entry is read to the end whatever the sink or the
  // `AbortSignal` says. Nothing in the descriptor is host-dependent, so the
  // output stays deterministic.
  dataDescriptor: true,
  // No extra field whose content depends on the host clock or filesystem.
  extendedTimestamp: false,
  ntfsTimestamp: false,
  msDosCompatible: true,
  useUnicodeFileNames: true,
});

/** Fallback archive name when the label sanitizes down to nothing. */
export const ARCHIVE_FALLBACK_NAME = "bore-transfer";
/** Longest archive file name this app will propose to the browser. */
export const ARCHIVE_NAME_MAX = 120;

/**
 * The name proposed for the saved archive. LOCAL only: it renames nothing
 * inside the archive, where the paths are already validated by the manifest
 * rules, and it never travels.
 */
export function archiveName(label) {
  const base = String(label ?? "")
    // Path separators and control characters are what a file name must not
    // contain; everything else the user chose is kept.
    .replace(CONTROL_OR_SEPARATOR, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, ARCHIVE_NAME_MAX);
  return `${base === "" || base === "." || base === ".." ? ARCHIVE_FALLBACK_NAME : base}.zip`;
}

/** Path separators plus C0/DEL: what a file name must not contain. */
const CONTROL_OR_SEPARATOR = new RegExp("[\\\\/\\u0000-\\u001f\\u007f]", "g");

/**
 * A zip.js `Reader` over a `File`, duck-typed: `size` plus `readUint8Array`
 * is the whole interface, and reading through `slice` is what keeps a large
 * entry off the heap. Importing zip.js's own `BlobReader` would pull the
 * reader half of the library -- and its codecs -- into the bundle for
 * nothing.
 */
export function fileReader(file) {
  return {
    size: file.size,
    async readUint8Array(index, length) {
      return new Uint8Array(
        await file.slice(index, index + length).arrayBuffer(),
      );
    },
  };
}

/**
 * The bounded sink: a `WritableStream` that cuts the archive into logical
 * chunks of exactly `chunkBytes` (the last one short) and awaits `onChunk`
 * before accepting more.
 *
 * Each chunk is a FRESH array. Handing out a view of a reused buffer would
 * be faster by one copy per megabyte and wrong the moment the consumer
 * awaits anything -- which the frame sender does, on every fragment.
 *
 * @returns `{ writable, bytes, chunks, buffered }` -- the counters are the
 * archive's running length and chunk count, which the FINAL frame must
 * carry because neither is in the manifest.
 */
export function createChunkSink({ onChunk, chunkBytes = CHUNK_BYTES } = {}) {
  if (typeof onChunk !== "function") {
    throw new Error("the chunk sink needs an onChunk consumer");
  }
  if (!Number.isSafeInteger(chunkBytes) || chunkBytes <= 0) {
    throw new Error("chunk size must be a positive safe integer");
  }
  const buffer = new Uint8Array(chunkBytes);
  let filled = 0;
  let chunks = 0;
  let bytes = 0;

  async function flush() {
    if (filled === 0) {
      return;
    }
    const out = buffer.slice(0, filled);
    filled = 0;
    const index = chunks;
    chunks += 1;
    await onChunk(out, index);
  }

  const writable = new WritableStream({
    async write(chunk) {
      let view =
        chunk instanceof Uint8Array
          ? chunk
          : new Uint8Array(
              chunk.buffer ?? chunk,
              chunk.byteOffset ?? 0,
              chunk.byteLength,
            );
      bytes += view.length;
      while (view.length > 0) {
        const take = Math.min(chunkBytes - filled, view.length);
        buffer.set(view.subarray(0, take), filled);
        filled += take;
        view = view.subarray(take);
        if (filled === chunkBytes) {
          await flush();
        }
      }
    },
    async close() {
      await flush();
    },
  });

  return {
    writable,
    get bytes() {
      return bytes;
    },
    get chunks() {
      return chunks;
    },
    get buffered() {
      return filled;
    },
  };
}

/**
 * Writes the whole offer as one archive, entries in MANIFEST order,
 * directories included.
 *
 * `fileFor(path)` returns the `File` for a manifest path; a path with no
 * file, or one whose size no longer matches the manifest, is a source that
 * changed under the offer and aborts before anything is finalized --
 * `close()` is never reached, so no central directory is written and the
 * recipient cannot mistake a truncated archive for a complete one.
 */
export async function writeArchive({
  entries,
  fileFor,
  writable,
  signal = null,
}) {
  const writer = new ZipWriter(writable, {
    ...ZIP_WRITER_OPTIONS,
    ...(signal === null ? {} : { signal }),
  });
  for (const entry of entries) {
    // The manifest's mtime, not the file's: the manifest is signed and the
    // file on disk is whatever it is now.
    const lastModDate = new Date(Number(entry.mtime ?? 0) * 1000);
    const isDirectory = entry.root === null || entry.root === undefined;
    if (isDirectory) {
      await writer.add(entry.path, null, { directory: true, lastModDate });
      continue;
    }
    const file = fileFor(entry.path);
    if (file === null || file === undefined) {
      throw Object.assign(new Error("source file missing"), {
        code: "SOURCE_CHANGED",
      });
    }
    if (Number(file.size) !== Number(entry.size)) {
      throw Object.assign(new Error("source file resized"), {
        code: "SOURCE_CHANGED",
      });
    }
    await writer.add(entry.path, fileReader(file), { lastModDate });
  }
  await writer.close();
}
