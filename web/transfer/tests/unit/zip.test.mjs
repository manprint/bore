import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  Uint8ArrayReader,
  Uint8ArrayWriter,
  ZipReader,
  configure,
} from "@zip.js/zip.js";

import {
  ARCHIVE_FALLBACK_NAME,
  ARCHIVE_NAME_MAX,
  ZIP_WRITER_OPTIONS,
  archiveName,
  createChunkSink,
  writeArchive,
} from "../../src/zip-stream.js";
import { CHUNK_BYTES } from "../../src/framing.js";
import {
  bytesToHex,
  fileRoot,
  hexToBytes,
  sha256Hex,
} from "../../src/crypto.js";

// The reader side is TEST-ONLY: it never enters the bundle (the app imports
// `lib/zip-core-writer.js` alone), and it is what makes these assertions
// about the FORMAT rather than about our own writer calls.
configure({ useWebWorkers: false });

/** A logical chunk small enough to make the sink's cutting observable. */
const SMALL_CHUNK = 64 * 1024;

function bytesOf(pattern, length) {
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) {
    out[i] = (pattern + i * 31) & 0xff;
  }
  return out;
}

function concat(parts) {
  let total = 0;
  for (const part of parts) {
    total += part.length;
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

function fileOf(bytes, name) {
  return new File([bytes], name, { lastModified: 1_600_000_000_000 });
}

/** Manifest-shaped file entry. */
function fileEntry(id, path, bytes, mtimeSec = 1_600_000_000) {
  return {
    id: String(id),
    path,
    size: String(bytes.length),
    mtime: String(mtimeSec),
    root: "ab".repeat(32),
  };
}

/** Manifest-shaped directory entry: the one with no root. */
function directoryEntry(id, path, mtimeSec = 1_600_000_000) {
  return {
    id: String(id),
    path,
    size: "0",
    mtime: String(mtimeSec),
    root: null,
  };
}

/**
 * Runs one archive through the bounded sink and returns everything the
 * assertions need: the chunks as they were handed out, the whole archive,
 * and the sink's own counters.
 */
async function buildArchive(entries, files, options = {}) {
  const { chunkBytes = SMALL_CHUNK, onChunk = null } = options;
  const chunks = [];
  const sink = createChunkSink({
    chunkBytes,
    onChunk: async (chunk, index) => {
      chunks.push(chunk);
      if (onChunk !== null) {
        await onChunk(chunk, index, sink);
      }
    },
  });
  await writeArchive({
    entries,
    fileFor: (path) => files.get(path) ?? null,
    writable: sink.writable,
  });
  return { chunks, bytes: concat(chunks), sink };
}

async function readEntries(bytes) {
  const reader = new ZipReader(new Uint8ArrayReader(bytes));
  const list = await reader.getEntries();
  const out = [];
  for (const entry of list) {
    out.push({
      filename: entry.filename,
      directory: entry.directory === true,
      size: Number(entry.uncompressedSize),
      data:
        entry.directory === true
          ? null
          : await entry.getData(new Uint8ArrayWriter()),
    });
  }
  await reader.close();
  return out;
}

describe("web-transfer zip", () => {
  it("zip_options_are_store_zip64_no_worker_and_no_variable_extras", async () => {
    // These options ARE a contract with the recipient: a resumed download
    // regenerates the archive and compares it with what it already
    // verified, so a changed option changes every archive ever produced.
    assert.equal(
      ZIP_WRITER_OPTIONS.level,
      0,
      "store only: the payload is already sealed",
    );
    assert.equal(ZIP_WRITER_OPTIONS.zip64, true, "always, not only past 4 GiB");
    assert.equal(ZIP_WRITER_OPTIONS.useWebWorkers, false);
    assert.equal(ZIP_WRITER_OPTIONS.useCompressionStream, false);
    assert.equal(
      ZIP_WRITER_OPTIONS.bufferedWrite,
      false,
      "never hold a whole entry",
    );
    assert.equal(ZIP_WRITER_OPTIONS.keepOrder, true);
    // ON, and for a memory reason rather than a format one: without it
    // zip.js buffers each whole entry through an INFINITY-highWaterMark
    // stream and this sink's backpressure never reaches the reader.
    assert.equal(ZIP_WRITER_OPTIONS.dataDescriptor, true);
    // Everything whose value would come from the host rather than from the
    // signed manifest.
    assert.equal(ZIP_WRITER_OPTIONS.extendedTimestamp, false);
    assert.equal(ZIP_WRITER_OPTIONS.ntfsTimestamp, false);
    assert.equal(ZIP_WRITER_OPTIONS.msDosCompatible, true);
    assert.equal(ZIP_WRITER_OPTIONS.useUnicodeFileNames, true);
    assert.ok(Object.isFrozen(ZIP_WRITER_OPTIONS), "a contract, not a default");
    // And the output carries no comment and no host-dependent extra field.
    const bytes = bytesOf(7, 1024);
    const { bytes: archive } = await buildArchive(
      [fileEntry(0, "a.bin", bytes)],
      new Map([["a.bin", fileOf(bytes, "a.bin")]]),
    );
    const reader = new ZipReader(new Uint8ArrayReader(archive));
    const [entry] = await reader.getEntries();
    await reader.close();
    assert.equal(entry.comment, "");
    assert.equal(entry.compressionMethod, 0, "stored");
    assert.equal(entry.zip64, true);
    assert.equal(entry.extraFieldExtendedTimestamp, undefined);
    assert.equal(entry.extraFieldNTFS, undefined);
  });

  it("zip_entries_follow_manifest_order_with_safe_paths", async () => {
    // Manifest order, not sorted order and not filesystem order: the
    // archive is regenerated from the manifest on every attempt, so the
    // manifest is the only ordering both ends can agree on.
    const alpha = bytesOf(1, 100);
    const beta = bytesOf(2, 200);
    const gamma = bytesOf(3, 300);
    const entries = [
      fileEntry(0, "cartella/z.bin", alpha),
      directoryEntry(1, "cartella/vuota"),
      fileEntry(2, "cartella/a.bin", beta),
      fileEntry(3, "b.bin", gamma),
    ];
    const files = new Map([
      ["cartella/z.bin", fileOf(alpha, "z.bin")],
      ["cartella/a.bin", fileOf(beta, "a.bin")],
      ["b.bin", fileOf(gamma, "b.bin")],
    ]);
    const { bytes } = await buildArchive(entries, files);
    const read = await readEntries(bytes);
    assert.deepEqual(
      read.map((entry) => entry.filename),
      ["cartella/z.bin", "cartella/vuota/", "cartella/a.bin", "b.bin"],
    );
    assert.deepEqual(
      read.map((entry) => entry.directory),
      [false, true, false, false],
    );
    // The paths are the manifest's own, unchanged: no leading slash, no
    // parent segment, no backslash.
    for (const entry of read) {
      assert.ok(!entry.filename.startsWith("/"));
      assert.ok(!entry.filename.includes(".."));
      assert.ok(!entry.filename.includes("\\"));
    }
    assert.deepEqual([...read[0].data], [...alpha]);
    assert.deepEqual([...read[2].data], [...beta]);
  });

  it("same_manifest_and_files_generate_identical_bytes_twice", async () => {
    // A resumed archive is regenerated from byte zero and compared against
    // what the recipient already verified: two generations that differ by
    // one byte would make every resume fail.
    const payload = bytesOf(5, 3 * SMALL_CHUNK + 17);
    const entries = [
      fileEntry(0, "dir/grande.bin", payload),
      directoryEntry(1, "dir/vuota"),
    ];
    const files = () =>
      new Map([["dir/grande.bin", fileOf(payload, "grande.bin")]]);
    const first = await buildArchive(entries, files());
    const second = await buildArchive(entries, files());
    assert.equal(first.bytes.length, second.bytes.length);
    assert.equal(await sha256Hex(first.bytes), await sha256Hex(second.bytes));
    // Also across a DIFFERENT chunking: the sink cuts the stream, it does
    // not shape it.
    const third = await buildArchive(entries, files(), { chunkBytes: 4096 });
    assert.equal(await sha256Hex(first.bytes), await sha256Hex(third.bytes));
    // And the File's own lastModified never reaches the output — only the
    // manifest's mtime does, because the manifest is signed.
    const restamped = new Map([
      [
        "dir/grande.bin",
        new File([payload], "grande.bin", { lastModified: 111_000 }),
      ],
    ]);
    const fourth = await buildArchive(entries, restamped);
    assert.equal(await sha256Hex(first.bytes), await sha256Hex(fourth.bytes));
  });

  it("zip_sink_never_buffers_more_than_one_logical_chunk", async () => {
    const payload = bytesOf(9, 5 * SMALL_CHUNK + 1234);
    let inFlight = 0;
    let maxInFlight = 0;
    let maxBuffered = 0;
    const seen = [];
    const { chunks, sink, bytes } = await buildArchive(
      [fileEntry(0, "grande.bin", payload)],
      new Map([["grande.bin", fileOf(payload, "grande.bin")]]),
      {
        onChunk: async (chunk, _index, live) => {
          inFlight += 1;
          maxInFlight = Math.max(maxInFlight, inFlight);
          // Yield: a sink that did not AWAIT the consumer would start the
          // next chunk here, and `maxInFlight` would read 2.
          await new Promise((resolve) => setTimeout(resolve, 0));
          maxBuffered = Math.max(maxBuffered, live.buffered);
          seen.push(chunk.length);
          inFlight -= 1;
        },
      },
    );
    assert.equal(maxInFlight, 1, "the writer waits for the consumer");
    assert.ok(
      maxBuffered <= SMALL_CHUNK,
      `buffered ${maxBuffered} beyond one chunk`,
    );
    // Every chunk is full but the last, which is what makes the recipient's
    // chunk cutting a pure function of the position.
    for (const length of seen.slice(0, -1)) {
      assert.equal(length, SMALL_CHUNK);
    }
    assert.ok(seen[seen.length - 1] <= SMALL_CHUNK);
    assert.equal(sink.chunks, chunks.length);
    assert.equal(sink.bytes, bytes.length);
    assert.equal(sink.buffered, 0, "nothing is held after close");
    // Each chunk is a FRESH array: handing out a view of a reused buffer
    // would be wrong the moment the consumer awaits, which it always does.
    const buffers = new Set(chunks.map((chunk) => chunk.buffer));
    assert.equal(buffers.size, chunks.length);
  });

  it("the_source_is_read_no_further_ahead_than_one_logical_chunk", async () => {
    // The red-check for the option above, and the only assertion that can
    // catch it: with `dataDescriptor: false` the archive still comes out
    // byte-identical and the sink still hands out one chunk at a time —
    // the whole entry is simply held in a stream nobody can see. What
    // changes, and all that changes, is how far the READER runs ahead.
    const SIZE = 16 * 1024 * 1024;
    let read = 0;
    const lazy = {
      size: SIZE,
      slice(start, end) {
        return {
          arrayBuffer: async () => {
            read += end - start;
            return new ArrayBuffer(end - start);
          },
        };
      },
    };
    let written = 0;
    let maxAhead = 0;
    const sink = createChunkSink({
      // The production chunk, because the bound below is in those units.
      chunkBytes: CHUNK_BYTES,
      onChunk: async (chunk) => {
        written += chunk.length;
        maxAhead = Math.max(maxAhead, read - written);
        // The consumer is slower than the producer, which is the ordinary
        // case (the frame sender seals and waits on the transport).
        await new Promise((resolve) => setTimeout(resolve, 0));
        maxAhead = Math.max(maxAhead, read - written);
      },
    });
    await writeArchive({
      entries: [
        {
          id: "0",
          path: "grande.bin",
          size: String(SIZE),
          mtime: "1600000000",
          root: "ab".repeat(32),
        },
      ],
      fileFor: () => lazy,
      writable: sink.writable,
    });
    assert.equal(read, SIZE, "the source is read exactly once");
    // zip.js reads in 512 KiB blocks of its own, so the bound is a couple
    // of logical chunks and not zero; the defect this refuses is the
    // reader finishing the WHOLE entry while the sink is still on its
    // first chunk — measured at 63.0 MiB of 64 MiB before the fix.
    assert.ok(
      maxAhead <= 2 * CHUNK_BYTES,
      `the reader ran ${maxAhead} bytes ahead of the sink`,
    );
  });

  it("an_abandoned_attempt_stops_reading_the_source", async () => {
    // A direct attempt that dies, a cancelled download and a withdrawn
    // offer all end the same way: the archive is abandoned. Abandoning it
    // must stop the reads, not merely stop caring about them — a buffered
    // entry is read to the end whatever the sink says, which on a large
    // offer is minutes of disk for an answer nobody will hear.
    const SIZE = 512 * 1024 * 1024;
    let read = 0;
    const lazy = {
      size: SIZE,
      slice(start, end) {
        return {
          arrayBuffer: async () => {
            read += end - start;
            return new ArrayBuffer(end - start);
          },
        };
      },
    };
    const controller = new AbortController();
    const sink = createChunkSink({
      chunkBytes: SMALL_CHUNK,
      onChunk: async () => {
        controller.abort();
      },
    });
    await assert.rejects(
      writeArchive({
        entries: [
          {
            id: "0",
            path: "grande.bin",
            size: String(SIZE),
            mtime: "1600000000",
            root: "ab".repeat(32),
          },
        ],
        fileFor: () => lazy,
        writable: sink.writable,
        signal: controller.signal,
      }),
      (error) => error?.name === "AbortError",
    );
    assert.ok(read < SIZE / 8, `${read} bytes were read after the abort`);
  });

  it("zip_root_and_length_cover_full_archive", async () => {
    const payload = bytesOf(3, 2 * SMALL_CHUNK + 999);
    const { chunks, bytes, sink } = await buildArchive(
      [fileEntry(0, "dir/a.bin", payload), directoryEntry(1, "dir/vuota")],
      new Map([["dir/a.bin", fileOf(payload, "a.bin")]]),
    );
    // The root the FINAL frame carries is computed over the chunks as they
    // were handed out …
    const streamed = [];
    for (const chunk of chunks) {
      streamed.push(hexToBytes(await sha256Hex(chunk)));
    }
    const streamedRoot = bytesToHex(await fileRoot(streamed.length, streamed));
    // … and it must equal the root of the SAME archive cut independently
    // from its whole bytes: central directory included, nothing lost before
    // the first chunk or after the last.
    const independent = [];
    for (let at = 0; at < bytes.length; at += SMALL_CHUNK) {
      independent.push(
        hexToBytes(await sha256Hex(bytes.subarray(at, at + SMALL_CHUNK))),
      );
    }
    assert.equal(
      streamedRoot,
      bytesToHex(await fileRoot(independent.length, independent)),
    );
    assert.equal(sink.bytes, bytes.length);
    assert.equal(sink.chunks, chunks.length);
    // The archive really is complete: it opens and the payload matches.
    const read = await readEntries(bytes);
    assert.equal(read.length, 2);
    assert.deepEqual([...read[0].data], [...payload]);
  });

  it("zip64_records_are_used_for_synthetic_large_source_without_large_allocation", async () => {
    // 5 GiB declared, none of it produced: the local header is written
    // BEFORE the payload, so the ZIP64 record can be read off the first
    // chunk and the run stops there. Lowering the production threshold to
    // make this cheap is exactly what `zip64: true` already avoids.
    const HUGE = 5 * 1024 * 1024 * 1024;
    const lazy = {
      size: HUGE,
      slice(start, end) {
        return { arrayBuffer: async () => new ArrayBuffer(end - start) };
      },
    };
    const head = [];
    let produced = 0;
    let peak = 0;
    const sink = createChunkSink({
      chunkBytes: SMALL_CHUNK,
      onChunk: async (chunk) => {
        head.push(chunk);
        produced += chunk.length;
        peak = Math.max(peak, chunk.length);
        throw Object.assign(new Error("enough"), { code: "STOP" });
      },
    });
    await assert.rejects(
      writeArchive({
        entries: [
          {
            id: "0",
            path: "grande.bin",
            size: String(HUGE),
            mtime: "1600000000",
            root: "ab".repeat(32),
          },
        ],
        fileFor: () => lazy,
        writable: sink.writable,
      }),
      (error) => error?.code === "STOP",
    );
    assert.equal(
      produced,
      SMALL_CHUNK,
      "one chunk was enough to read the header",
    );
    assert.ok(
      peak <= SMALL_CHUNK,
      "nothing larger than one logical chunk was ever held",
    );
    const bytes = head[0];
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    assert.equal(view.getUint32(0, true), 0x04034b50, "local file header");
    assert.equal(view.getUint16(4, true), 45, "version needed: ZIP64");
    // Both 32-bit size fields are the ZIP64 escape value …
    assert.equal(view.getUint32(18, true), 0xffffffff);
    assert.equal(view.getUint32(22, true), 0xffffffff);
    const nameLength = view.getUint16(26, true);
    const extraLength = view.getUint16(28, true);
    assert.ok(
      extraLength >= 20,
      "room for the ZIP64 extended information field",
    );
    const extraAt = 30 + nameLength;
    // … and the extended-information field carries the real 64-bit sizes.
    assert.equal(
      view.getUint16(extraAt, true),
      0x0001,
      "ZIP64 extended information",
    );
    assert.equal(Number(view.getBigUint64(extraAt + 4, true)), HUGE);
    assert.equal(Number(view.getBigUint64(extraAt + 12, true)), HUGE);
  });

  it("source_change_aborts_before_successful_central_directory", async () => {
    // A file that changed under the offer must not produce a SHORTER
    // archive that still opens: `close()` is what writes the central
    // directory, and it is never reached.
    const payload = bytesOf(4, 2048);
    const entries = [
      fileEntry(0, "a.bin", payload),
      fileEntry(1, "b.bin", payload),
    ];
    const resized = new Map([
      ["a.bin", fileOf(payload, "a.bin")],
      ["b.bin", fileOf(bytesOf(4, 1024), "b.bin")],
    ]);
    const chunks = [];
    const sink = createChunkSink({
      chunkBytes: SMALL_CHUNK,
      onChunk: async (chunk) => {
        chunks.push(chunk);
      },
    });
    await assert.rejects(
      writeArchive({
        entries,
        fileFor: (path) => resized.get(path) ?? null,
        writable: sink.writable,
      }),
      (error) => error?.code === "SOURCE_CHANGED",
    );
    await assert.rejects(
      readEntries(concat(chunks)),
      "a truncated archive must not open",
    );
    // A file that vanished is the same answer, and neither case finalizes.
    const missing = new Map([["a.bin", fileOf(payload, "a.bin")]]);
    const other = createChunkSink({
      chunkBytes: SMALL_CHUNK,
      onChunk: async () => {},
    });
    await assert.rejects(
      writeArchive({
        entries,
        fileFor: (path) => missing.get(path) ?? null,
        writable: other.writable,
      }),
      (error) => error?.code === "SOURCE_CHANGED",
    );
  });

  it("archive_name_sanitization_is_local_and_bounded", async () => {
    assert.equal(archiveName("foto vacanze"), "foto vacanze.zip");
    // Separators and control characters are what a file NAME must not hold;
    // the rest of the user's label survives.
    assert.equal(archiveName("a/b\\c"), "a b c.zip");
    assert.equal(archiveName("nome x"), "nome x.zip");
    assert.equal(archiveName("  spazi  "), "spazi.zip");
    for (const empty of ["", "   ", "/", ".", "..", null, undefined]) {
      assert.equal(archiveName(empty), `${ARCHIVE_FALLBACK_NAME}.zip`);
    }
    const long = "n".repeat(500);
    assert.equal(archiveName(long).length, ARCHIVE_NAME_MAX + 4);
    // LOCAL only: the paths INSIDE the archive are the manifest's, and a
    // label that would be a bad file name renames nothing in there.
    const payload = bytesOf(6, 32);
    const { bytes } = await buildArchive(
      [fileEntry(0, "cartella/a.bin", payload)],
      new Map([["cartella/a.bin", fileOf(payload, "a.bin")]]),
    );
    const read = await readEntries(bytes);
    assert.deepEqual(
      read.map((entry) => entry.filename),
      ["cartella/a.bin"],
    );
  });

  it("empty_directory_and_empty_file_round_trip", async () => {
    // The two cases a byte count cannot tell apart, and the two the RAW
    // download path cannot serve at all: an empty directory exists only as
    // an archive entry, and an empty file has no chunk to verify.
    const empty = new Uint8Array(0);
    const entries = [
      directoryEntry(0, "vuota"),
      directoryEntry(1, "a/b/c"),
      fileEntry(2, "zero.bin", empty),
      fileEntry(3, "a/b/c/anche-qui.bin", empty),
    ];
    const files = new Map([
      ["zero.bin", fileOf(empty, "zero.bin")],
      ["a/b/c/anche-qui.bin", fileOf(empty, "anche-qui.bin")],
    ]);
    const { bytes, sink } = await buildArchive(entries, files);
    assert.ok(sink.bytes > 0, "an archive of nothing is still an archive");
    const read = await readEntries(bytes);
    assert.deepEqual(
      read.map((entry) => [entry.filename, entry.directory, entry.size]),
      [
        ["vuota/", true, 0],
        ["a/b/c/", true, 0],
        ["zero.bin", false, 0],
        ["a/b/c/anche-qui.bin", false, 0],
      ],
    );
    assert.equal(read[2].data.length, 0);
    assert.equal(read[3].data.length, 0);
  });

  it("the_logical_chunk_is_the_manifest_hashing_unit", () => {
    // The default is not a tuning knob: the recipient cuts the archive at
    // the same boundary to rebuild the leaves, and the FINAL frame's chunk
    // count is in these units.
    const sink = createChunkSink({ onChunk: async () => {} });
    assert.equal(sink.buffered, 0);
    assert.equal(CHUNK_BYTES, 1024 * 1024);
    assert.throws(() => createChunkSink({}), /onChunk/);
    assert.throws(
      () => createChunkSink({ onChunk: async () => {}, chunkBytes: 0 }),
      /positive/,
    );
  });
});
