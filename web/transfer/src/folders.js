// Directory selection adapters: the two browser ways to choose a tree, plus
// drag-and-drop, all reduced to ONE shape the offer worker can hash.
//
// The shape is an array of entries:
//   { file, path }                     a file, path relative to the root
//   { directory: true, path, mtimeSec } a directory with nothing under it
//
// Only EMPTY directories get an entry of their own: every other directory is
// implied by the paths of the files under it, and a ZIP writer creates the
// parents it needs. An empty directory is the one thing that would otherwise
// be lost, and the manifest has a shape for exactly it (size 0, no chunks,
// null root — rejected by the server unless the offer kind is `folder`).
//
// Nothing here reads file CONTENT: it walks names and metadata and hands the
// `File` objects on. Hashing stays in the worker.

/** Hard cap on entries in one selection (the server's own default). */
export const FOLDER_MAX_ENTRIES = 10000;
/** Hard cap on directory nesting below the selected root. */
export const FOLDER_MAX_DEPTH = 128;

/** A directory has no observable timestamp through this API. */
const DIRECTORY_MTIME_SEC = 0;

function abortError() {
  const error = new Error("selezione annullata");
  error.aborted = true;
  return error;
}

/** Throws the abort error the offer pipeline already understands. */
function checkAborted(signal) {
  if (signal !== null && signal !== undefined && signal.aborted) {
    throw abortError();
  }
}

/** Code-point order over NFC, the same order the manifest is sorted in. */
function byName(a, b) {
  const left = a.normalize("NFC");
  const right = b.normalize("NFC");
  return left < right ? -1 : left > right ? 1 : 0;
}

/** True when this engine can open a directory picker. */
export function supportsDirectoryPicker(win = globalThis) {
  return typeof win?.showDirectoryPicker === "function";
}

/**
 * Opens the directory picker. This module is the ONE place that names the
 * API (a static tripwire in the unit suite enforces it), and the function is
 * deliberately synchronous: it must be called from inside the click handler
 * because the picker consumes the user activation, and awaiting anything
 * before it loses the activation and the call is refused.
 *
 * @returns the engine's promise of a `FileSystemDirectoryHandle`
 */
export function pickDirectory(win = globalThis) {
  return win.showDirectoryPicker();
}

/**
 * The `<input webkitdirectory>` / plain-picker path: a flat `FileList` whose
 * members carry their own relative path.
 *
 * Empty directories are INVISIBLE here — the API never reports them — so a
 * caller that needs to tell the user about that limitation asks
 * {@link supportsDirectoryPicker} and gets `false`.
 */
export function entriesFromFileList(files) {
  return [...files].map((file) => {
    const relative =
      typeof file.webkitRelativePath === "string" && file.webkitRelativePath !== ""
        ? file.webkitRelativePath
        : file.name;
    return { file, path: relative.startsWith("/") ? relative.slice(1) : relative };
  });
}

/**
 * Walks a `FileSystemDirectoryHandle` depth-first, collecting the names of
 * one level before descending so the order is the manifest's and not the
 * engine's.
 *
 * Refuses anything that is not a `file` or a `directory` handle and never
 * follows it: a symlink, a mount point or a future handle kind is a thing
 * this code has not been written against, and walking into it is how a
 * traversal leaves the tree the user picked.
 *
 * @param {object} handle directory handle (duck-typed in tests)
 * @param {object} [options] `{ maxEntries, maxDepth, signal, rootName }`
 * @returns array of entries, unsorted paths already rooted at the folder name
 */
export async function entriesFromDirectoryHandle(handle, options = {}) {
  const maxEntries = options.maxEntries ?? FOLDER_MAX_ENTRIES;
  const maxDepth = options.maxDepth ?? FOLDER_MAX_DEPTH;
  const signal = options.signal ?? null;
  const rootName = options.rootName ?? handle?.name ?? "";
  if (typeof rootName !== "string" || rootName === "") {
    throw new Error("la cartella selezionata non ha un nome");
  }
  const entries = [];

  function push(entry) {
    entries.push(entry);
    if (entries.length > maxEntries) {
      throw new Error("Troppe voci per una sola offerta");
    }
  }

  // Returns how many FILES live at or below `dir`; a subtree with none is
  // what earns a directory entry of its own.
  async function walk(dir, prefix, depth) {
    checkAborted(signal);
    if (depth > maxDepth) {
      throw new Error("Cartella troppo annidata");
    }
    // Metadata first, then order: an engine may enumerate in any order, and
    // the manifest's order is part of what both ends agree on.
    const children = [];
    for await (const child of dir.values()) {
      checkAborted(signal);
      children.push(child);
      if (children.length > maxEntries) {
        throw new Error("Troppe voci per una sola offerta");
      }
    }
    children.sort((a, b) => byName(a.name, b.name));
    let files = 0;
    for (const child of children) {
      checkAborted(signal);
      const path = `${prefix}${child.name}`;
      if (child.kind === "file") {
        push({ file: await child.getFile(), path });
        files += 1;
      } else if (child.kind === "directory") {
        // A directory earns an entry only when the whole subtree below it
        // produced nothing at all: any entry under it — a file OR a deeper
        // empty directory — already implies it through its own path, and a
        // manifest entry with a null root means "this directory is empty",
        // which would then be a lie.
        const before = entries.length;
        const below = await walk(child, `${path}/`, depth + 1);
        if (entries.length === before) {
          push({ directory: true, path, mtimeSec: DIRECTORY_MTIME_SEC });
        }
        files += below;
      } else {
        throw new Error("Voce di cartella non supportata");
      }
    }
    return files;
  }

  const files = await walk(handle, `${rootName}/`, 1);
  if (files === 0 && entries.length === 0) {
    // A completely empty root still has to describe itself, or the manifest
    // would have no entries at all and the server would refuse it.
    push({ directory: true, path: rootName, mtimeSec: DIRECTORY_MTIME_SEC });
  }
  return entries;
}

/**
 * The drop path. Directories are walked when the engine exposes handles
 * (`DataTransferItem.getAsFileSystemHandle`); otherwise the drop carries
 * flat files only, which is stated rather than silently assumed.
 *
 * @returns `{ entries, directories, flat }` — `flat` is true when the engine
 * gave us no handles, so the caller can say so instead of pretending a
 * dropped folder was empty.
 */
export async function entriesFromDataTransfer(dataTransfer, options = {}) {
  const items = dataTransfer?.items ? [...dataTransfer.items] : [];
  const handles = [];
  for (const item of items) {
    if (item.kind !== "file" || typeof item.getAsFileSystemHandle !== "function") {
      continue;
    }
    // Must be called synchronously-ish while the drop data is alive; the
    // promise itself may settle later.
    handles.push(item.getAsFileSystemHandle());
  }
  if (handles.length === 0) {
    return {
      entries: entriesFromFileList(dataTransfer?.files ?? []),
      directories: 0,
      flat: true,
    };
  }
  // `null` AND `undefined`: the spec says a handle or null, and an engine
  // that cannot produce one for a given item has been seen answering
  // `undefined`. Both mean the same thing here, and letting `undefined`
  // through reaches the `kind` check below as "unsupported entry" — a
  // refusal for something the user did nothing wrong with.
  const resolved = (await Promise.all(handles)).filter(
    (handle) => handle !== null && handle !== undefined,
  );
  if (resolved.length === 0) {
    // The engine offered handles and then resolved every one of them to
    // `null`. That happens for real — a drop the engine will not expose as
    // a handle at all — and the files are still right there on the
    // `DataTransfer`. Falling back to them is the difference between
    // publishing what was dropped and publishing nothing while saying
    // nothing, which is the one outcome a drop must never produce.
    return {
      entries: entriesFromFileList(dataTransfer?.files ?? []),
      directories: 0,
      flat: true,
    };
  }
  const entries = [];
  let directories = 0;
  for (const handle of resolved.sort((a, b) => byName(a.name ?? "", b.name ?? ""))) {
    if (handle.kind === "file") {
      entries.push({ file: await handle.getFile(), path: handle.name });
    } else if (handle.kind === "directory") {
      directories += 1;
      entries.push(...(await entriesFromDirectoryHandle(handle, options)));
    } else {
      throw new Error("Voce di cartella non supportata");
    }
  }
  return { entries, directories, flat: false };
}

/**
 * Builds the catalog tree from a manifest's entries — names and sizes only,
 * which is the whole point: a recipient renders a tree BEFORE it has asked
 * for a single byte, and nothing here opens, reads or fetches anything.
 *
 * Directory entries (`root: null`) are the empty ones; every other directory
 * is implied by the paths under it and is created here on the way down, so
 * the tree a recipient sees is the tree the source walked.
 *
 * @param {Array} entries manifest entries `{ path, size, root }`
 * @returns `{ name, directory, children, files, bytes }`, children sorted by
 * NFC code point with directories and files in one list, exactly as the
 * manifest is sorted.
 */
export function treeFromManifestEntries(entries = []) {
  const root = { name: "", directory: true, children: new Map(), files: 0, bytes: 0 };
  for (const entry of entries) {
    const parts = String(entry?.path ?? "")
      .split("/")
      .filter((part) => part !== "");
    if (parts.length === 0) {
      continue;
    }
    // A null root is the manifest's word for "directory"; anything else is a
    // file and carries a size.
    const isDirectory = entry?.root === null || entry?.root === undefined;
    let node = root;
    parts.forEach((part, index) => {
      const last = index === parts.length - 1;
      let child = node.children.get(part);
      if (child === undefined) {
        child = {
          name: part,
          directory: !last || isDirectory,
          children: new Map(),
          files: 0,
          bytes: 0,
          // Manifest identity, carried on FILE nodes only: it is what a
          // per-file download puts in `entryIds`, and the server's `raw`
          // selection rule is exactly one FILE entry of this manifest, so
          // the node the user clicks is the whole of the request.
          id: null,
          path: null,
        };
        node.children.set(part, child);
      }
      if (last && !isDirectory) {
        child.directory = false;
        child.bytes = Number(entry?.size ?? 0) || 0;
        child.files = 1;
        child.id = String(entry?.id ?? "");
        child.path = String(entry?.path ?? "");
      }
      node = child;
    });
  }

  // Aggregate on the way back up, then freeze the order: a Map preserves
  // insertion order, and insertion order is the manifest's only when no
  // directory was implied, so the sort is not optional.
  function fold(node) {
    const children = [...node.children.values()].sort((a, b) => byName(a.name, b.name));
    let files = node.directory ? 0 : 1;
    let bytes = node.directory ? 0 : node.bytes;
    for (const child of children) {
      const folded = fold(child);
      files += folded.files;
      bytes += folded.bytes;
    }
    node.children = children;
    node.files = files;
    node.bytes = bytes;
    return node;
  }
  return fold(root);
}
