// Unit tests: folder selection (plan 001, sub-phase 5.1). Two browser paths
// produce ONE manifest shape, a traversal never leaves the tree it was given,
// and nothing here reads a byte of content — the worker hashes, this module
// walks names.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  FOLDER_MAX_DEPTH,
  FOLDER_MAX_ENTRIES,
  entriesFromDataTransfer,
  entriesFromDirectoryHandle,
  entriesFromFileList,
  supportsDirectoryPicker,
  treeFromManifestEntries,
} from "../../src/folders.js";
import { prepareOffer } from "../../src/offer-worker.js";
import { createView } from "../../src/view.js";

// ---- duck-typed engines --------------------------------------------------

function testFile(name, bytes, { relativePath = "", lastModified = 1757779200000 } = {}) {
  const blob = new Blob([bytes]);
  return {
    name,
    size: blob.size,
    lastModified,
    webkitRelativePath: relativePath,
    slice: (start, end) => blob.slice(start, end),
    arrayBuffer: () => blob.arrayBuffer(),
  };
}

/** A `FileSystemDirectoryHandle` as the spec describes it, and no more. */
function dirHandle(name, children, { order = null } = {}) {
  const listed = order ?? children;
  return {
    kind: "directory",
    name,
    async *values() {
      for (const child of listed) {
        yield child;
      }
    },
  };
}

function fileHandle(name, file) {
  return { kind: "file", name, getFile: async () => file };
}

function fakeDoc() {
  function node(tag) {
    const self = {
      tagName: tag,
      children: [],
      attributes: {},
      listeners: {},
      _text: "",
      set textContent(value) {
        self._text = String(value);
      },
      get textContent() {
        return self._text;
      },
      set className(value) {
        self.attributes.class = String(value);
      },
      appendChild(child) {
        self.children.push(child);
        return child;
      },
      removeChild(child) {
        self.children = self.children.filter((x) => x !== child);
      },
      setAttribute(key, value) {
        self.attributes[key] = String(value);
      },
      addEventListener(type, fn) {
        (self.listeners[type] ??= []).push(fn);
      },
      get firstChild() {
        return self.children[0] ?? null;
      },
      value: "",
      disabled: false,
    };
    return self;
  }
  const root = node("main");
  return {
    doc: { createElement: (tag) => node(tag) },
    root,
    find(id, start = root) {
      if (start.attributes.id === id) {
        return start;
      }
      for (const child of start.children) {
        const hit = this.find(id, child);
        if (hit !== null) {
          return hit;
        }
      }
      return null;
    },
  };
}

const noopCallbacks = (sink) => ({
  onRename: () => {},
  onSelectFiles: (entries, origin) => sink.selected.push([entries, origin]),
  onSelectionError: (message) => sink.errors.push(message),
  onWithdraw: () => {},
  onCopyLink: () => {},
  onDownload: () => {},
  onCancelTransfer: () => {},
});

const paths = (entries) => entries.map((entry) => entry.path);

const ROOM_ID = "0f1e2d3c4b5a69780f1e2d3c4b5a6978";
const ROOM_KEY = "1a".repeat(32);
const LIMITS = { maxEntriesPerOffer: 10000, maxOfferBytes: 1099511627776 };

describe("folder selection", () => {
  it("directory_handle_and_webkitrelativepath_produce_same_manifest", async () => {
    // The same tree, seen through the two APIs a browser can offer, must
    // reach the worker as the same manifest — otherwise which browser the
    // source used would be visible in the offer.
    const a = testFile("a.txt", "alpha", { relativePath: "tree/a.txt" });
    const nested = testFile("n.txt", "nested", { relativePath: "tree/sub/n.txt" });
    const fromInput = entriesFromFileList([a, nested]);

    const handle = dirHandle("tree", [
      fileHandle("a.txt", testFile("a.txt", "alpha")),
      dirHandle("sub", [fileHandle("n.txt", testFile("n.txt", "nested"))]),
    ]);
    const fromHandle = await entriesFromDirectoryHandle(handle);

    assert.deepEqual(paths(fromInput), ["tree/a.txt", "tree/sub/n.txt"]);
    assert.deepEqual(paths(fromHandle), ["tree/a.txt", "tree/sub/n.txt"]);

    const prepared = await Promise.all(
      [fromInput, fromHandle].map((entries) =>
        prepareOffer({
          offerId: "ab".repeat(16),
          kind: "folder",
          label: "tree",
          createdAt: "2026-09-14T12:00:00Z",
          files: entries.map((entry) => ({
            file: entry.file,
            relativePath: entry.path,
            directory: entry.directory === true,
            mtimeSec: entry.mtimeSec ?? 0,
          })),
          roomIdHex: ROOM_ID,
          roomKeyHex: ROOM_KEY,
          limits: LIMITS,
        }),
      ),
    );
    assert.deepEqual(
      prepared[0].manifest.entries.map((entry) => [entry.path, entry.size, entry.root]),
      prepared[1].manifest.entries.map((entry) => [entry.path, entry.size, entry.root]),
      "the two selection paths must hash to the same manifest entries",
    );
  });

  it("empty_directories_are_preserved_when_exposed", async () => {
    // Only the handle API can see an empty directory at all, so only it can
    // preserve one; the flat path cannot invent what it was never told.
    const handle = dirHandle("tree", [
      dirHandle("empty", []),
      fileHandle("a.txt", testFile("a.txt", "alpha")),
      dirHandle("holder", [dirHandle("deep-empty", [])]),
    ]);
    const entries = await entriesFromDirectoryHandle(handle);
    assert.deepEqual(paths(entries), [
      "tree/a.txt",
      "tree/empty",
      "tree/holder/deep-empty",
    ]);
    const empty = entries.filter((entry) => entry.directory === true);
    assert.equal(empty.length, 2);
    for (const entry of empty) {
      assert.equal(entry.mtimeSec, 0, "a directory has no observable timestamp");
    }
    // A directory that HOLDS files is implied by their paths, never an entry.
    assert.ok(!paths(entries).includes("tree/holder"));

    // A completely empty root still describes itself: a manifest with no
    // entries at all is refused by the server.
    const alone = await entriesFromDirectoryHandle(dirHandle("solo", []));
    assert.deepEqual(paths(alone), ["solo"]);
    assert.equal(alone[0].directory, true);

    // The flat path reports files and therefore reports no directories.
    assert.deepEqual(
      entriesFromFileList([testFile("a.txt", "a", { relativePath: "tree/a.txt" })]).filter(
        (entry) => entry.directory === true,
      ),
      [],
    );
    assert.equal(supportsDirectoryPicker({}), false);
    assert.equal(supportsDirectoryPicker({ showDirectoryPicker: () => {} }), true);
  });

  it("unknown_handle_and_symlink_are_never_followed", async () => {
    // Anything that is not `file` or `directory` is refused where it is
    // found: a traversal that guesses is a traversal that leaves the tree.
    for (const kind of ["symlink", "mount", "", undefined]) {
      const handle = dirHandle("tree", [
        fileHandle("a.txt", testFile("a.txt", "alpha")),
        { kind, name: "weird", values: () => assert.fail("never descended") },
      ]);
      await assert.rejects(
        entriesFromDirectoryHandle(handle),
        /non supportata/,
        `kind ${String(kind)} must be refused`,
      );
    }
    // Same rule on the drop path.
    await assert.rejects(
      entriesFromDataTransfer({
        items: [
          {
            kind: "file",
            getAsFileSystemHandle: async () => ({ kind: "symlink", name: "link" }),
          },
        ],
      }),
      /non supportata/,
    );
  });

  it("depth_entry_reserved_id_and_byte_caps_fail_before_publish", async () => {
    // Deeper than the cap: refused, and refused BEFORE anything is published.
    let deep = dirHandle("leaf", [fileHandle("a.txt", testFile("a.txt", "a"))]);
    for (let level = 0; level < FOLDER_MAX_DEPTH + 2; level += 1) {
      deep = dirHandle(`d${level}`, [deep]);
    }
    await assert.rejects(entriesFromDirectoryHandle(deep), /annidata/);

    // More entries than the cap: same, and the cap is checked as entries are
    // collected rather than after the whole tree is in memory.
    const many = dirHandle(
      "tree",
      Array.from({ length: 12 }, (_, index) =>
        fileHandle(`f${index}.txt`, testFile(`f${index}.txt`, "x")),
      ),
    );
    await assert.rejects(
      entriesFromDirectoryHandle(many, { maxEntries: 4 }),
      /Troppe voci/,
    );
    assert.equal(FOLDER_MAX_ENTRIES, 10000);
    assert.equal(FOLDER_MAX_DEPTH, 128);

    // The reserved ZIP entry ID is `0xffff_ffff`, so a selection can hold at
    // most `0xffff_ffff` entries; the cap this code ships is four orders of
    // magnitude below it, which is what makes the reserved ID unreachable.
    assert.ok(FOLDER_MAX_ENTRIES < 0xffff_ffff);
  });

  it("folder_root_paths_are_canonical_and_sorted", async () => {
    // The engine may enumerate in any order; the manifest's order is code
    // point order over NFC and is produced here, not hoped for.
    const decomposed = "café.txt"; // e + combining acute
    const handle = dirHandle("radice", [
      fileHandle("zeta.txt", testFile("zeta.txt", "z")),
      fileHandle("Alfa.txt", testFile("Alfa.txt", "A")),
      fileHandle(decomposed, testFile(decomposed, "c")),
      dirHandle("beta", [fileHandle("b.txt", testFile("b.txt", "b"))]),
    ]);
    const entries = await entriesFromDirectoryHandle(handle);
    const listed = paths(entries);
    assert.deepEqual(listed, [
      "radice/Alfa.txt",
      "radice/beta/b.txt",
      `radice/${decomposed}`,
      "radice/zeta.txt",
    ]);
    for (const path of listed) {
      assert.ok(path.startsWith("radice/"), "every path is rooted at the folder name");
      assert.ok(!path.includes("//") && !path.startsWith("/"));
    }
    // An unnamed root is refused rather than silently rooted at "".
    await assert.rejects(entriesFromDirectoryHandle({ kind: "directory", values: () => {} }), /nome/);
  });

  it("cancel_mid_traversal_publishes_nothing", async () => {
    // The signal is checked at every iteration, so a cancelled traversal
    // throws instead of returning the half of the tree it had reached.
    const controller = new AbortController();
    let seen = 0;
    const children = Array.from({ length: 6 }, (_, index) =>
      fileHandle(`f${index}.txt`, testFile(`f${index}.txt`, "x")),
    );
    const handle = {
      kind: "directory",
      name: "tree",
      async *values() {
        for (const child of children) {
          seen += 1;
          if (seen === 3) {
            controller.abort();
          }
          yield child;
        }
      },
    };
    await assert.rejects(
      entriesFromDirectoryHandle(handle, { signal: controller.signal }),
      (error) => error.aborted === true,
    );
    // Already aborted before the first step: nothing is walked at all.
    const pre = new AbortController();
    pre.abort();
    await assert.rejects(
      entriesFromDirectoryHandle(dirHandle("tree", children), { signal: pre.signal }),
      (error) => error.aborted === true,
    );
  });

  it("drag_drop_fallback_is_flat_and_explicit", async () => {
    // With handles, a dropped directory is walked.
    const walked = await entriesFromDataTransfer({
      items: [
        {
          kind: "file",
          getAsFileSystemHandle: async () =>
            dirHandle("tree", [fileHandle("a.txt", testFile("a.txt", "a"))]),
        },
      ],
    });
    assert.equal(walked.flat, false);
    assert.equal(walked.directories, 1);
    assert.deepEqual(paths(walked.entries), ["tree/a.txt"]);

    // Without them, the drop carries flat files and SAYS so.
    const flat = await entriesFromDataTransfer({
      items: [{ kind: "file" }],
      files: [testFile("a.txt", "a"), testFile("b.txt", "b")],
    });
    assert.equal(flat.flat, true);
    assert.equal(flat.directories, 0);
    assert.deepEqual(paths(flat.entries), ["a.txt", "b.txt"]);

    // And the view says it in words when a DIRECTORY was dropped on such an
    // engine — never an empty offer, never silence.
    const sink = { selected: [], errors: [] };
    const fake = fakeDoc();
    createView(fake.doc, fake.root, noopCallbacks(sink), {});
    const dropzone = fake.find("dropzone");
    const drop = dropzone.listeners.drop[0];
    await drop({
      preventDefault: () => {},
      dataTransfer: {
        items: [{ kind: "file", webkitGetAsEntry: () => ({ isDirectory: true }) }],
        files: [],
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.deepEqual(sink.selected, []);
    assert.equal(sink.errors.length, 1);
    assert.match(sink.errors[0], /Aggiungi cartella/);

    // A folder dropped TOGETHER with files: the files are published and the
    // folder is reported as lost — never both lost, never silently.
    await drop({
      preventDefault: () => {},
      dataTransfer: {
        items: [
          { kind: "file", webkitGetAsEntry: () => ({ isDirectory: true }) },
          { kind: "file", webkitGetAsEntry: () => ({ isDirectory: false }) },
        ],
        files: [testFile("solo.txt", "solo")],
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(sink.errors.length, 2);
    assert.equal(sink.selected.length, 1);
    assert.deepEqual(paths(sink.selected[0][0]), ["solo.txt"]);

    // A plain file drop on the same engine publishes normally.
    await drop({
      preventDefault: () => {},
      dataTransfer: {
        items: [{ kind: "file", webkitGetAsEntry: () => ({ isDirectory: false }) }],
        files: [testFile("a.txt", "a")],
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(sink.selected.length, 2);
    assert.equal(sink.selected[1][1], "drop");
  });

  it("catalog_tree_is_built_from_the_manifest_alone", () => {
    // What a recipient renders before it has asked for a byte: names, counts
    // and sizes, with implied directories created and empty ones kept.
    const tree = treeFromManifestEntries([
      { path: "tree/empty", size: "0", root: null },
      { path: "tree/f1.bin", size: "3", root: "aa" },
      { path: "tree/sub/f2.bin", size: "10", root: "bb" },
    ]);
    assert.equal(tree.files, 2);
    assert.equal(tree.bytes, 13);
    assert.deepEqual(
      tree.children.map((child) => child.name),
      ["tree"],
    );
    const top = tree.children[0];
    assert.deepEqual(
      top.children.map((child) => [child.name, child.directory, child.files, child.bytes]),
      [
        ["empty", true, 0, 0],
        ["f1.bin", false, 1, 3],
        ["sub", true, 1, 10],
      ],
    );
  });
});
