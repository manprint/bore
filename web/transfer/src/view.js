// DOM renderer: status, own name, peers and grouped catalog.
//
// Remote strings (display names, labels, paths) enter the document ONLY via
// `textContent` (or string-only properties like `button.disabled`); never
// `innerHTML`, never attribute interpolation. `doc` is injected so Node
// tests drive the renderer with a recording fake — pass `document` in
// production. Links carry `rel="noopener"`.
import {
  entriesFromDataTransfer,
  entriesFromDirectoryHandle,
  pickDirectory,
  supportsDirectoryPicker,
  treeFromManifestEntries,
} from "./folders.js";
import {
  PATH,
  TRANSFER,
  canCancelTransfer,
  sortedEntries,
  transferPercent,
} from "./state.js";

// Unicode BIDIRECTIONAL CONTROL characters are the one way a string that is
// already inert as markup can still lie about itself: `fattura\u202Efdp.exe`
// renders as `fattura exe.pdf`, and the reader decides whether to download
// from exactly that text. They are stripped rather than escaped because this
// UI has no legitimate use for them — a name, a label and a path are read,
// never laid out — and because stripping is what `textContent` cannot do for
// us. `el()` plus the few direct `textContent` assignments below are the ONE
// funnel every remote string passes through; keep it that way instead of
// sanitizing at each call site.
const BIDI_CONTROLS = /[\u061C\u200E\u200F\u202A-\u202E\u2066-\u2069]/gu;

/**
 * @param {unknown} value any value on its way into the document
 * @returns {unknown} the same value with bidi overrides removed from strings
 */
export function inertText(value) {
  return typeof value === "string" ? value.replace(BIDI_CONTROLS, "") : value;
}

function el(doc, tag, attrs, text) {
  const node = doc.createElement(tag);
  if (attrs) {
    for (const key of Object.keys(attrs)) {
      if (key === "class") {
        node.className = attrs[key];
      } else if (key.startsWith("on") && typeof attrs[key] === "function") {
        node.addEventListener(key.slice(2), attrs[key]);
      } else if (attrs[key] === true) {
        node.setAttribute(key, "");
      } else if (
        attrs[key] !== false &&
        attrs[key] !== null &&
        attrs[key] !== undefined
      ) {
        node.setAttribute(key, String(attrs[key]));
      }
    }
  }
  if (text !== null && text !== undefined) {
    node.textContent = inertText(text);
  }
  return node;
}

function clear(node) {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

/**
 * @param {Document} doc document (or recording fake in tests)
 * @param {Element} root mount point, cleared on every render
 * @param {object} callbacks `{ onRename(name), onSelectFiles(files, origin),
 * onWithdraw(offerId), onCopyLink(), onDownload(offerId, mode, entryId),
 * onRestartDownload(offerId, mode), onCancelTransfer(transferId) }` with
 * origin `"picker"|"folder"|"drop"`
 */
export function createView(
  doc,
  root,
  callbacks,
  win = doc?.defaultView ?? globalThis,
) {
  clear(root);
  root.appendChild(el(doc, "h1", null, "Bore Transfer"));
  const status = el(doc, "p", { id: "room-status", role: "status" });
  const toast = el(doc, "p", {
    id: "app-toast",
    role: "status",
    "aria-live": "polite",
    class: "visually-hidden",
  });
  root.appendChild(status);
  root.appendChild(toast);

  // Test-only development note (2.4): the TEXT comes from the
  // `__BORE_TEST__` hook installed before app load, so the production
  // bundle never contains the label. Real users see no note element at all.
  const hookNote =
    typeof globalThis !== "undefined" &&
    globalThis !== null &&
    typeof globalThis.__BORE_TEST__ === "object" &&
    globalThis.__BORE_TEST__ !== null &&
    typeof globalThis.__BORE_TEST__.transferNote === "string"
      ? globalThis.__BORE_TEST__.transferNote
      : null;

  // Three stable zones (5.6): "la mia room", "offerte", "trasferimenti".
  // They are created and appended ONCE, in this order, and nothing ever
  // moves between them afterwards — a control that changes position while a
  // transfer runs is a control the user has to find again, and the DOM order
  // here IS the visual order, which is what makes the tab order match it.
  const roomZone = el(doc, "section", {
    id: "zone-room",
    class: "zone",
    "aria-label": "La mia room",
  });
  roomZone.appendChild(el(doc, "h2", null, "La mia room"));
  const offersZone = el(doc, "section", {
    id: "zone-offers",
    class: "zone",
    "aria-label": "Offerte",
  });
  offersZone.appendChild(el(doc, "h2", null, "Offerte"));
  const transfersZone = el(doc, "section", {
    id: "zone-transfers",
    class: "zone",
    "aria-label": "Trasferimenti",
  });
  transfersZone.appendChild(el(doc, "h2", null, "Trasferimenti"));
  root.appendChild(roomZone);
  root.appendChild(offersZone);
  root.appendChild(transfersZone);

  const nameSection = el(doc, "section", { "aria-label": "Il tuo nome" });
  const nameForm = el(doc, "form", { id: "rename-form" });
  const nameInput = el(doc, "input", {
    id: "rename-input",
    name: "displayName",
    maxlength: "48",
    autocomplete: "off",
  });
  const nameButton = el(doc, "button", { type: "submit" }, "Rinomina");
  nameForm.appendChild(nameInput);
  nameForm.appendChild(nameButton);
  nameForm.addEventListener("submit", (event) => {
    event.preventDefault();
    callbacks.onRename(nameInput.value);
  });
  nameSection.appendChild(nameForm);
  roomZone.appendChild(nameSection);

  const peersSection = el(doc, "section", { "aria-label": "Peer" });
  const peersTitle = el(doc, "h3", null, "Peer");
  const peerList = el(doc, "ul", { id: "peer-list" });
  peersSection.appendChild(peersTitle);
  peersSection.appendChild(peerList);
  roomZone.appendChild(peersSection);

  // Built here, appended to its zone AFTER the share controls below: the
  // gesture that adds an offer belongs above the list of offers.
  const catalogSection = el(doc, "section", { "aria-label": "Catalogo" });
  const catalogTitle = el(doc, "h3", null, "Catalogo");
  const catalog = el(doc, "div", { id: "catalog" });
  catalogSection.appendChild(catalogTitle);
  catalogSection.appendChild(catalog);

  // Live transfers (3.5). A row exists only for a transfer THIS tab is a
  // party to, and only a click can create one — the section is empty until
  // then, which is what `T-WEB-NOAUTO` counts.
  const transfersSection = el(doc, "section", {
    "aria-label": "Trasferimenti attivi",
  });
  const transferList = el(doc, "div", { id: "transfers" });
  transfersSection.appendChild(transferList);
  transfersZone.appendChild(transfersSection);

  const shareSection = el(doc, "section", { "aria-label": "Condivisione" });
  const addFile = el(
    doc,
    "button",
    { id: "add-file", type: "button" },
    "Aggiungi file",
  );
  const fileInput = el(doc, "input", {
    id: "file-input",
    type: "file",
    multiple: true,
    class: "visually-hidden",
    tabindex: "-1",
    "aria-hidden": "true",
  });
  addFile.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    callbacks.onSelectFiles([...fileInput.files], "picker");
    fileInput.value = "";
  });
  const addFolder = el(
    doc,
    "button",
    { id: "add-folder", type: "button" },
    "Aggiungi cartella",
  );
  const folderInput = el(doc, "input", {
    id: "folder-input",
    type: "file",
    class: "visually-hidden",
    tabindex: "-1",
    "aria-hidden": "true",
  });
  folderInput.setAttribute("webkitdirectory", "");
  addFolder.addEventListener("click", () => {
    // `showDirectoryPicker()` must be called from inside the click itself:
    // it consumes the user activation, and awaiting anything first loses it.
    // It is preferred where it exists because it is the only API that can
    // report an EMPTY directory — `webkitdirectory` reports files and
    // therefore cannot describe one.
    if (supportsDirectoryPicker(win)) {
      let picked;
      try {
        picked = pickDirectory(win);
      } catch {
        folderInput.click();
        return;
      }
      picked
        .then((handle) => entriesFromDirectoryHandle(handle))
        .then((entries) => callbacks.onSelectFiles(entries, "folder"))
        .catch((error) => {
          // The user closing the picker is not a failure and says nothing.
          if (error?.name === "AbortError") {
            return;
          }
          callbacks.onSelectionError?.(String(error?.message ?? error));
        });
      return;
    }
    folderInput.click();
  });
  folderInput.addEventListener("change", () => {
    callbacks.onSelectFiles([...folderInput.files], "folder");
    folderInput.value = "";
  });
  const dropzone = el(
    doc,
    "div",
    {
      id: "dropzone",
      tabindex: "0",
      role: "button",
      "aria-label": "Trascina qui file o cartelle",
    },
    "Trascina qui file o cartelle",
  );
  // Drag feedback (5.6). `dragenter`/`dragover` must BOTH preventDefault or
  // the engine keeps its own "not allowed" cursor and the drop never fires;
  // the class is what makes the target visible while a drag is over it, and
  // it is removed on `dragleave` and on `drop` alike so a cancelled drag
  // cannot leave the zone lit.
  let dropLocked = false;
  const setDragging = (on) => {
    dropzone.setAttribute("data-dragging", on ? "true" : "false");
    dropzone.className = on ? "is-dragging" : "";
  };
  setDragging(false);
  dropzone.addEventListener("dragenter", (event) => {
    event.preventDefault();
    setDragging(true);
  });
  dropzone.addEventListener("dragover", (event) => {
    event.preventDefault();
    setDragging(true);
  });
  dropzone.addEventListener("dragleave", () => setDragging(false));
  dropzone.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      fileInput.click();
    }
  });
  dropzone.addEventListener("drop", (event) => {
    event.preventDefault();
    setDragging(false);
    if (dropLocked) {
      // A room that cannot publish must SAY so: a drop that produced
      // nothing and no message reads as a broken page.
      callbacks.onSelectionError?.(
        "Room non disponibile: non è possibile pubblicare adesso",
      );
      return;
    }
    // A dropped DIRECTORY is only walkable where the engine exposes handles.
    // Where it does not, the drop carries flat files and that is SAID, never
    // silently turned into an empty offer.
    const transfer = event.dataTransfer;
    // Asked HERE, synchronously: a `DataTransferItemList` is only alive for
    // the duration of the event handler, so a question asked after the first
    // await answers about a list the engine has already emptied.
    const droppedDirectory = transfer?.items
      ? [...transfer.items].some(
          (item) =>
            item.kind === "file" &&
            item.webkitGetAsEntry?.()?.isDirectory === true,
        )
      : false;
    entriesFromDataTransfer(transfer).then(
      ({ entries, directories, flat }) => {
        if (flat && directories === 0 && droppedDirectory) {
          // The folder is lost either way on this engine; saying so and
          // still publishing the files that DID arrive is better than
          // refusing everything, and better than publishing in silence.
          callbacks.onSelectionError?.(
            "Questo browser non consente di trascinare cartelle: usa Aggiungi cartella",
          );
          if (entries.length === 0) {
            return;
          }
        }
        callbacks.onSelectFiles(entries, directories > 0 ? "folder" : "drop");
      },
      (error) => callbacks.onSelectionError?.(String(error?.message ?? error)),
    );
  });
  const copyLink = el(
    doc,
    "button",
    { id: "copy-link", type: "button" },
    "Copia link room",
  );
  copyLink.addEventListener("click", () => callbacks.onCopyLink());
  shareSection.appendChild(addFile);
  shareSection.appendChild(fileInput);
  shareSection.appendChild(addFolder);
  shareSection.appendChild(folderInput);
  shareSection.appendChild(dropzone);
  offersZone.appendChild(shareSection);
  offersZone.appendChild(catalogSection);
  // The link is how the room GROWS, so it lives with the room and not with
  // the offers.
  roomZone.appendChild(copyLink);

  let note = null;
  if (hookNote !== null) {
    note = el(doc, "p", { "data-testid": "transfer-note" }, hookNote);
    root.appendChild(note);
  }

  // Verified-file panel (3.4): appears after a download verifies, offers an
  // explicit save or discard. Filenames travel via textContent only, exactly
  // like every other remote string.
  const saveSection = el(doc, "section", {
    id: "save-section",
    "aria-label": "File verificato",
  });
  const saveName = el(doc, "p", { id: "save-name" });
  const saveButton = el(
    doc,
    "button",
    { id: "save-file", type: "button" },
    "Salva file verificato",
  );
  const discardButton = el(
    doc,
    "button",
    {
      id: "discard-file",
      type: "button",
      title:
        "Elimina i byte già verificati senza salvarli: il download andrà rifatto da capo",
    },
    "Scarta",
  );
  saveSection.appendChild(saveName);
  saveSection.appendChild(saveButton);
  saveSection.appendChild(discardButton);
  let saveHandlers = null;
  saveButton.addEventListener("click", () => {
    // User activation: the file-picker enhancement (when present) may only
    // run here; the anchor fallback below works everywhere.
    if (saveHandlers !== null) {
      saveHandlers.onSave();
    }
  });
  discardButton.addEventListener("click", () => {
    if (saveHandlers !== null) {
      saveHandlers.onDiscard();
    }
  });

  const STATE_TEXT = new Map([
    [TRANSFER.REQUESTING, "In attesa della sorgente"],
    [TRANSFER.TRANSFERRING, "Trasferimento in corso"],
    [TRANSFER.VERIFIED, "Verificato, da salvare"],
    [TRANSFER.DONE, "Completato"],
    [TRANSFER.CANCELLED, "Annullato"],
    [TRANSFER.FAILED, "Non riuscito"],
  ]);

  /** The path badge's words. `connecting` is honest, not a placeholder. */
  const PATH_TEXT = new Map([
    [PATH.CONNECTING, "in connessione"],
    [PATH.DIRECT, "diretto"],
    [PATH.RELAY, "relay"],
  ]);

  /**
   * A SHAPE beside the word, because colour alone is not a distinction:
   * roughly one reader in twelve cannot use it, and a screen capture, a
   * printed page and a high-contrast theme all lose it too. The glyph is
   * `aria-hidden` — it repeats what the word already says, and a screen
   * reader should hear the word once.
   */
  const PATH_MARK = new Map([
    [PATH.CONNECTING, "◌"],
    [PATH.DIRECT, "◆"],
    [PATH.RELAY, "▲"],
  ]);

  /** One line each, on the badge itself: what the transport actually means. */
  const PATH_TITLE = new Map([
    [
      PATH.CONNECTING,
      "Il percorso non è ancora un fatto: nessun blocco è stato verificato su un trasporto",
    ],
    [
      PATH.DIRECT,
      "I byte viaggiano da browser a browser: il server non li vede",
    ],
    [
      PATH.RELAY,
      "I byte passano cifrati dal server bore, che non può leggerli",
    ],
  ]);

  /** Bytes as a short human string (row metadata only, never a log line). */
  function humanBytes(value) {
    const bytes = Number(value ?? 0);
    if (!Number.isFinite(bytes) || bytes <= 0) {
      return "0 B";
    }
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let n = bytes;
    let unit = 0;
    while (n >= 1024 && unit < units.length - 1) {
      n /= 1024;
      unit += 1;
    }
    return `${unit === 0 ? Math.round(n) : n.toFixed(1)} ${units[unit]}`;
  }

  /** Directories holding at most this many files render already expanded. */
  const TREE_OPEN_MAX_FILES = 50;

  /**
   * Renders one offer's tree from its MANIFEST — names, counts and sizes,
   * never a byte of content. Expansion is a native `<details>` disclosure
   * rather than a hand-rolled `role="tree"`: the native element is keyboard
   * operable and announced correctly in every engine we ship on, and a tree
   * widget that is only half-implemented is worse for a screen reader than
   * the plain disclosure it replaced. Directories are closed by default so a
   * 10 000-entry offer does not arrive as a wall of names.
   */
  function renderTree(tree, file = null) {
    function renderNode(node) {
      const item = el(doc, "li", {
        class: node.directory ? "tree-dir" : "tree-file",
      });
      if (!node.directory) {
        item.appendChild(el(doc, "span", { class: "tree-name" }, node.name));
        item.appendChild(
          el(doc, "span", { class: "tree-size" }, humanBytes(node.bytes)),
        );
        // One file of a folder, on its own. The server has always allowed
        // it (`raw` is exactly one FILE entry of this manifest, whichever
        // one); what was missing was a gesture for it, and without one the
        // only way to read a single file out of a folder was to download
        // the whole archive.
        if (file !== null && node.id !== null && node.bytes > 0) {
          const pick = el(
            doc,
            "button",
            {
              type: "button",
              class: "tree-download",
              "data-download-entry": `${file.offerId}:${node.id}`,
            },
            "Scarica",
          );
          pick.disabled = file.disabled;
          pick.addEventListener("click", () =>
            file.onPick(file.offerId, node.id),
          );
          item.appendChild(pick);
        }
        return item;
      }
      const details = el(doc, "details");
      // Open where the subtree is small enough to be worth showing at once:
      // a closed tree hides the whole offer behind a click, and an open one
      // with ten thousand entries is a wall of names. The budget counts
      // FILES, which is the number the user reads in the summary.
      if (node.files <= TREE_OPEN_MAX_FILES) {
        details.setAttribute("open", "");
      }
      const summary = el(doc, "summary", null, node.name);
      summary.appendChild(
        el(
          doc,
          "span",
          { class: "tree-size" },
          node.files === 0
            ? "vuota"
            : `${node.files} file · ${humanBytes(node.bytes)}`,
        ),
      );
      details.appendChild(summary);
      if (node.children.length > 0) {
        const list = el(doc, "ul", { class: "tree-children" });
        for (const child of node.children) {
          list.appendChild(renderNode(child));
        }
        details.appendChild(list);
      }
      item.appendChild(details);
      return item;
    }
    const list = el(doc, "ul", { class: "offer-tree" });
    for (const child of tree.children) {
      list.appendChild(renderNode(child));
    }
    return list;
  }

  // A button the user is reaching for must not be replaced under their
  // cursor. Progress events arrive many times a second, and a full rebuild
  // per event detaches every control: measured, a `Annulla` click never
  // landed on firefox or webkit (Playwright retried until the test timed
  // out) while chromium won the race by luck. So the catalog/peer list is
  // rebuilt only when its own inputs changed, and transfer rows are updated
  // in place, keyed by transfer ID.
  let catalogKey = null;
  /** transferId → the live row's nodes (created once, updated after). */
  const rowNodes = new Map();
  /**
   * The transfers zone's empty state. Attached and detached by hand rather
   * than rebuilt, for the same reason the rows are: this zone must not be
   * cleared while a transfer runs (§8.66 — a rebuild under an event detaches
   * the button the user is about to press). `attached` is tracked here
   * because the recording fake used by the unit tests has no `parentNode`.
   */
  const emptyTransfers = el(
    doc,
    "p",
    { id: "transfers-empty", class: "empty-state" },
    "Nessun trasferimento. Un download parte solo con un clic su un'offerta: niente si avvia da solo.",
  );
  let emptyTransfersAttached = false;

  function catalogSignature(state, local, locked) {
    return JSON.stringify([
      locked,
      state.selfPeerId ?? null,
      sortedEntries(state.peers).map(([id, peer]) => [
        id,
        peer.displayName ?? null,
      ]),
      sortedEntries(state.offers).map(([id, offer]) => [
        id,
        offer.peerId,
        offer.manifest?.label ?? null,
        offer.manifest?.kind ?? null,
        Array.isArray(offer.manifest?.entries)
          ? offer.manifest.entries.length
          : 0,
      ]),
      [...(state.resumable ?? [])].sort(),
      [...(state.sourceChanged ?? [])].sort(),
      [...state.transfers.values()]
        .filter(
          (row) =>
            row.state === TRANSFER.REQUESTING ||
            row.state === TRANSFER.TRANSFERRING,
        )
        .map((row) => row.offerId)
        .sort(),
      [...local].map(([id, ui]) => [
        id,
        ui.status,
        Math.round((ui.progress01 ?? 0) * 100),
      ]),
    ]);
  }

  function setStatus(text, { announce = false } = {}) {
    status.textContent = inertText(text);
    if (announce) {
      toast.textContent = inertText(text);
    }
  }

  return {
    /**
     * Full re-render from reducer state (small DOM; clarity over diffing).
     * `offersUi` maps offer IDs to local preparation state
     * (`{ status: "preparing"|"ready"|"live"|"error"|"invalid"|"withdrawing",
     * progress01 }`); absent entries render catalog data without controls.
     */
    render(state, offersUi) {
      const local = offersUi ?? new Map();
      setStatus(state.statusText);
      // Never clobber what the user is typing. `render` runs on every room
      // event — a peer joining, an offer arriving, a progress tick — so an
      // unconditional assignment emptied the field mid-word on a busy room,
      // and submitted the OLD name when it did not.
      if (doc.activeElement !== nameInput) {
        nameInput.value = state.displayName ?? "";
      }
      const locked =
        state.connection === "unavailable" || state.connection === "incomplete";
      for (const control of [
        nameInput,
        nameButton,
        addFile,
        addFolder,
        copyLink,
      ]) {
        control.disabled = locked;
      }
      dropzone.setAttribute("aria-disabled", locked ? "true" : "false");
      dropLocked = locked;

      const peerName = (id) => state.peers.get(id)?.displayName ?? id;
      const signature = catalogSignature(state, local, locked);
      const rebuildCatalog = signature !== catalogKey;
      catalogKey = signature;

      if (rebuildCatalog) {
        clear(peerList);
        for (const [peerId, peer] of sortedEntries(state.peers)) {
          const label = peer.displayName ?? peerId;
          const item = el(
            doc,
            "li",
            null,
            peerId === state.selfPeerId ? `${label} (tu)` : label,
          );
          item.setAttribute("data-peer", peerId);
          peerList.appendChild(item);
        }

        clear(catalog);
        const offers = sortedEntries(state.offers);
        if (offers.length === 0) {
          // An empty state says what to DO, not merely that something is
          // absent: an empty room is the first thing every user sees.
          catalog.appendChild(
            el(
              doc,
              "p",
              { id: "catalog-empty", class: "empty-state" },
              locked
                ? "Room non disponibile: nessuna offerta da mostrare."
                : "Nessuna offerta. Aggiungi un file o una cartella, oppure trascinali qui: i byte restano in questa scheda finché qualcuno non li chiede.",
            ),
          );
        }
        // Grouped by owner: peers in ID order, each owner's offers in ID
        // order; orphan offers (owner already left) render last.
        const byOwner = new Map();
        for (const [offerId, offer] of offers) {
          if (!byOwner.has(offer.peerId)) {
            byOwner.set(offer.peerId, []);
          }
          byOwner.get(offer.peerId).push([offerId, offer]);
        }
        const renderGroup = (ownerId, list) => {
          const group = el(doc, "section", { class: "offer-group" });
          group.appendChild(el(doc, "h3", null, peerName(ownerId)));
          for (const [offerId, offer] of list) {
            const card = el(doc, "article", {
              class: "offer-card",
              "data-offer": offerId,
            });
            const title = offer.manifest?.label ?? offerId;
            card.appendChild(el(doc, "h4", null, title));
            const kind = offer.manifest?.kind ?? "";
            const manifestEntries = Array.isArray(offer.manifest?.entries)
              ? offer.manifest.entries
              : [];
            const tree = treeFromManifestEntries(manifestEntries);
            card.appendChild(
              el(
                doc,
                "p",
                { class: "offer-meta" },
                `${kind} · ${tree.files} file · ${humanBytes(tree.bytes)}`,
              ),
            );
            const mine = ownerId === state.selfPeerId;
            const resumable = state.resumable?.has(offerId) === true;
            const changed = state.sourceChanged?.has(offerId) === true;
            const live = [...state.transfers.values()].some(
              (row) =>
                row.offerId === offerId &&
                (row.state === TRANSFER.REQUESTING ||
                  row.state === TRANSFER.TRANSFERRING),
            );
            if (kind !== "file" && manifestEntries.length > 0) {
              card.appendChild(
                renderTree(
                  tree,
                  mine
                    ? null
                    : {
                        offerId,
                        disabled: locked || live,
                        onPick: (id, entryId) =>
                          callbacks.onDownload(id, "raw", entryId),
                      },
                ),
              );
            }
            const ui = local.get(offerId);
            if (
              ui &&
              (ui.status === "preparing" || ui.status === "withdrawing")
            ) {
              const bar = el(doc, "progress", {
                max: "100",
                value: String(Math.round((ui.progress01 ?? 0) * 100)),
              });
              card.appendChild(bar);
            }
            if (ui && ui.status === "error") {
              card.appendChild(
                el(
                  doc,
                  "p",
                  { class: "offer-error" },
                  "Preparazione non riuscita",
                ),
              );
            }
            if (ui && ui.status === "invalid") {
              card.appendChild(
                el(
                  doc,
                  "p",
                  { class: "offer-error" },
                  "File modificato: ripubblica",
                ),
              );
            }
            if (mine) {
              const withdraw = el(
                doc,
                "button",
                {
                  type: "button",
                  "data-withdraw": offerId,
                  title:
                    "Toglie l'offerta dalla room: nessuno potrà più chiederla e i trasferimenti in corso su di essa finiscono",
                },
                "Ritira",
              );
              withdraw.addEventListener("click", () =>
                callbacks.onWithdraw(offerId),
              );
              card.appendChild(withdraw);
            } else {
              // Someone else's offer: the offer-level button is what takes
              // the WHOLE offer, and it is the only place a download of the
              // whole offer can start. A partial on disk renames it to
              // "Riprendi" — the reconnect never resumes by itself.
              // WHICH download this offer supports is a property of the
              // offer, not a preference: a single file is served raw, and
              // anything else — several files, a folder, an empty file —
              // only exists on the recipient's disk as one archive. So the
              // card still shows exactly ONE button, and it says which.
              const rawEntries = manifestEntries.filter(
                (entry) =>
                  Array.isArray(entry.chunks) && entry.chunks.length > 0,
              );
              const zip =
                manifestEntries.length !== 1 || rawEntries.length !== 1;
              const download = el(
                doc,
                "button",
                zip
                  ? { type: "button", "data-download-zip": offerId }
                  : { type: "button", "data-download": offerId },
                zip ? "Scarica ZIP" : resumable ? "Riprendi" : "Scarica",
              );
              // A pending request disables its own button: a second click
              // would mint a second transfer for the same selection.
              download.disabled = locked || live;
              download.addEventListener("click", () =>
                callbacks.onDownload(offerId, zip ? "zip" : "raw"),
              );
              card.appendChild(download);
              if (resumable && !changed) {
                card.appendChild(
                  el(
                    doc,
                    "p",
                    { class: "offer-meta" },
                    "Disponibile per ripresa",
                  ),
                );
              }
              if (changed) {
                // The partial is still on disk and still verified; what no
                // longer matches is the source. Say that, and make the only
                // destructive way forward an explicit second click.
                card.appendChild(
                  el(
                    doc,
                    "p",
                    { class: "offer-error" },
                    "La sorgente è cambiata: la ripresa non è più valida",
                  ),
                );
                const restart = el(
                  doc,
                  "button",
                  { type: "button", "data-restart": offerId },
                  "Riparti da zero",
                );
                restart.disabled = locked || live;
                restart.addEventListener("click", () =>
                  callbacks.onRestartDownload(offerId, zip ? "zip" : "raw"),
                );
                card.appendChild(restart);
              }
            }
            group.appendChild(card);
          }
          catalog.appendChild(group);
        };
        for (const [peerId] of sortedEntries(state.peers)) {
          const list = byOwner.get(peerId);
          if (list && list.length > 0) {
            renderGroup(peerId, list);
            byOwner.delete(peerId);
          }
        }
        for (const [ownerId, list] of byOwner) {
          renderGroup(ownerId, list);
        }
      }

      // Transfer rows: create once, update in place, remove when gone. The
      // cancel button therefore survives every progress event.
      for (const [transferId, nodes] of rowNodes) {
        if (!state.transfers.has(transferId)) {
          transferList.removeChild(nodes.line);
          rowNodes.delete(transferId);
        }
      }
      if (state.transfers.size === 0 && !emptyTransfersAttached) {
        transferList.appendChild(emptyTransfers);
        emptyTransfersAttached = true;
      } else if (state.transfers.size > 0 && emptyTransfersAttached) {
        transferList.removeChild(emptyTransfers);
        emptyTransfersAttached = false;
      }
      for (const [transferId, row] of state.transfers) {
        let nodes = rowNodes.get(transferId);
        if (nodes === undefined) {
          const line = el(doc, "article", {
            class: "transfer-row",
            "data-transfer": transferId,
          });
          const title = el(doc, "h4", null, row.label ?? transferId);
          const meta = el(doc, "p", { class: "transfer-meta" });
          // The path is its OWN element so it can be styled and read back:
          // "is this direct or through the server" is the question this
          // feature exists to answer, and burying it in a sentence hides it.
          const info = el(doc, "span", { class: "transfer-info" });
          const path = el(doc, "span", { class: "transfer-path" });
          const pathMark = el(doc, "span", {
            class: "path-mark",
            "aria-hidden": "true",
          });
          const pathWord = el(doc, "span", { class: "path-word" });
          path.appendChild(pathMark);
          path.appendChild(pathWord);
          const bar = el(doc, "progress", {
            class: "transfer-progress",
            // `<progress>` already has the role, but a value an assistive
            // technology can read is not automatic: the three ARIA values
            // are what make the bar say "37 per cent" instead of "busy".
            role: "progressbar",
            "aria-valuemin": "0",
            "aria-valuemax": "100",
            "aria-valuenow": "0",
            max: "100",
            value: "0",
          });
          const stateLine = el(doc, "p", { class: "transfer-state" });
          const cancel = el(
            doc,
            "button",
            {
              type: "button",
              "data-cancel": transferId,
              // A destructive action says what it costs, before it is used.
              title:
                "Interrompe il trasferimento per entrambi i peer; i blocchi già verificati restano su disco",
            },
            "Annulla",
          );
          cancel.addEventListener("click", () =>
            callbacks.onCancelTransfer(transferId),
          );
          line.appendChild(title);
          meta.appendChild(info);
          meta.appendChild(path);
          line.appendChild(meta);
          line.appendChild(bar);
          line.appendChild(stateLine);
          nodes = {
            line,
            meta,
            info,
            path,
            pathMark,
            pathWord,
            bar,
            stateLine,
            cancel,
            cancelAttached: false,
          };
          rowNodes.set(transferId, nodes);
          transferList.appendChild(line);
        }
        const other =
          row.direction === "in" ? row.sourcePeerId : row.recipientPeerId;
        const direction = row.direction === "in" ? "Da" : "A";
        // Two spans, not one sentence: the badge is the answer to "direct or
        // through the server", and it has to be findable, styleable and
        // readable on its own.
        nodes.info.textContent = inertText(
          `${direction} ${peerName(other)} · ${humanBytes(row.totalBytes)} · `,
        );
        nodes.path.setAttribute("data-path", row.path);
        nodes.path.setAttribute("title", PATH_TITLE.get(row.path) ?? "");
        nodes.pathMark.textContent = PATH_MARK.get(row.path) ?? "";
        nodes.pathWord.textContent = PATH_TEXT.get(row.path) ?? row.path;
        const percent = transferPercent(row);
        nodes.bar.setAttribute("value", String(percent));
        nodes.bar.setAttribute("aria-valuenow", String(percent));
        nodes.bar.value = percent;
        const speed =
          row.bytesPerSecond === null || row.bytesPerSecond === undefined
            ? ""
            : ` · ${humanBytes(row.bytesPerSecond)}/s`;
        nodes.stateLine.setAttribute("data-state", row.state);
        nodes.stateLine.textContent = `${percent}% · ${
          STATE_TEXT.get(row.state) ?? row.state
        }${speed}`;
        const cancellable = canCancelTransfer(row, state.selfPeerId);
        nodes.cancel.disabled = locked;
        if (cancellable && !nodes.cancelAttached) {
          nodes.line.appendChild(nodes.cancel);
          nodes.cancelAttached = true;
        } else if (!cancellable && nodes.cancelAttached) {
          nodes.line.removeChild(nodes.cancel);
          nodes.cancelAttached = false;
        }
      }
    },
    setStatus,
    /**
     * In-app announcement (status changes, toasts); never the OS API.
     *
     * It is SEEN as well as announced. The element was `visually-hidden`,
     * so every message the app already produced — "Link copiato",
     * "Offerta non riuscita: …", "Download scartato" — reached a screen
     * reader and nobody else, which is the same as not producing it (5.6).
     * Empty text hides it again so an empty box never sits in the layout.
     */
    announce(text) {
      const message = inertText(String(text ?? ""));
      toast.textContent = message;
      toast.className = message === "" ? "visually-hidden" : "app-toast-live";
    },
    /**
     * Shows the verified-file panel for one staged download. Only one panel
     * is ever visible; a second staged file replaces the first (its staging
     * is purged by the caller first).
     */
    showVerifiedFile({ fileName, onSave, onDiscard }) {
      saveName.textContent = inertText(fileName);
      saveHandlers = { onSave, onDiscard };
      if (saveSection.parentNode !== root) {
        root.appendChild(saveSection);
      }
    },
    /** Hides the verified-file panel (after save, discard or purge). */
    hideVerifiedFile() {
      saveHandlers = null;
      if (saveSection.parentNode === root) {
        root.removeChild(saveSection);
      }
    },
  };
}
