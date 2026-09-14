// DOM renderer: status, own name, peers and grouped catalog.
//
// Remote strings (display names, labels, paths) enter the document ONLY via
// `textContent` (or string-only properties like `button.disabled`); never
// `innerHTML`, never attribute interpolation. `doc` is injected so Node
// tests drive the renderer with a recording fake — pass `document` in
// production. Links carry `rel="noopener"`.
import { sortedEntries } from "./state.js";

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
      } else if (attrs[key] !== false && attrs[key] !== null && attrs[key] !== undefined) {
        node.setAttribute(key, String(attrs[key]));
      }
    }
  }
  if (text !== null && text !== undefined) {
    node.textContent = text;
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
 * onWithdraw(offerId), onCopyLink() }` with origin `"picker"|"folder"|"drop"`
 */
export function createView(doc, root, callbacks) {
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
  root.appendChild(nameSection);

  const peersSection = el(doc, "section", { "aria-label": "Peer" });
  const peersTitle = el(doc, "h2", null, "Peer");
  const peerList = el(doc, "ul", { id: "peer-list" });
  peersSection.appendChild(peersTitle);
  peersSection.appendChild(peerList);
  root.appendChild(peersSection);

  const catalogSection = el(doc, "section", { "aria-label": "Catalogo" });
  const catalogTitle = el(doc, "h2", null, "Catalogo");
  const catalog = el(doc, "div", { id: "catalog" });
  catalogSection.appendChild(catalogTitle);
  catalogSection.appendChild(catalog);
  root.appendChild(catalogSection);

  const shareSection = el(doc, "section", { "aria-label": "Condivisione" });
  const addFile = el(doc, "button", { id: "add-file", type: "button" }, "Aggiungi file");
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
  const addFolder = el(doc, "button", { id: "add-folder", type: "button" }, "Aggiungi cartella");
  const folderInput = el(doc, "input", {
    id: "folder-input",
    type: "file",
    class: "visually-hidden",
    tabindex: "-1",
    "aria-hidden": "true",
  });
  folderInput.setAttribute("webkitdirectory", "");
  addFolder.addEventListener("click", () => folderInput.click());
  folderInput.addEventListener("change", () => {
    callbacks.onSelectFiles([...folderInput.files], "folder");
    folderInput.value = "";
  });
  const dropzone = el(
    doc,
    "div",
    { id: "dropzone", tabindex: "0", role: "button", "aria-label": "Trascina qui file o cartelle" },
    "Trascina qui file o cartelle",
  );
  dropzone.addEventListener("dragover", (event) => event.preventDefault());
  dropzone.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      fileInput.click();
    }
  });
  dropzone.addEventListener("drop", (event) => {
    event.preventDefault();
    // dataTransfer.files holds files only; directory traversal (full
    // subtree walks) lands in Phase 5, so anything here is flat input.
    const files = event.dataTransfer ? [...event.dataTransfer.files] : [];
    callbacks.onSelectFiles(files, "drop");
  });
  const copyLink = el(doc, "button", { id: "copy-link", type: "button" }, "Copia link room");
  copyLink.addEventListener("click", () => callbacks.onCopyLink());
  shareSection.appendChild(addFile);
  shareSection.appendChild(fileInput);
  shareSection.appendChild(addFolder);
  shareSection.appendChild(folderInput);
  shareSection.appendChild(dropzone);
  shareSection.appendChild(copyLink);
  root.appendChild(shareSection);

  let note = null;
  if (hookNote !== null) {
    note = el(doc, "p", { "data-testid": "transfer-note" }, hookNote);
    root.appendChild(note);
  }

  function setStatus(text, { announce = false } = {}) {
    status.textContent = text;
    if (announce) {
      toast.textContent = text;
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
      nameInput.value = state.displayName ?? "";
      const locked = state.connection === "unavailable" || state.connection === "incomplete";
      for (const control of [nameInput, nameButton, addFile, addFolder, copyLink]) {
        control.disabled = locked;
      }
      dropzone.setAttribute("aria-disabled", locked ? "true" : "false");

      clear(peerList);
      for (const [peerId, peer] of sortedEntries(state.peers)) {
        const label = peer.displayName ?? peerId;
        const item = el(doc, "li", null, peerId === state.selfPeerId ? `${label} (tu)` : label);
        item.setAttribute("data-peer", peerId);
        peerList.appendChild(item);
      }

      clear(catalog);
      const offers = sortedEntries(state.offers);
      if (offers.length === 0) {
        catalog.appendChild(el(doc, "p", { id: "catalog-empty" }, "Nessuna offerta."));
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
      const peerName = (id) => state.peers.get(id)?.displayName ?? id;
      const renderGroup = (ownerId, list) => {
        const group = el(doc, "section", { class: "offer-group" });
        group.appendChild(el(doc, "h3", null, peerName(ownerId)));
        for (const [offerId, offer] of list) {
          const card = el(doc, "article", { class: "offer-card", "data-offer": offerId });
          const title = offer.manifest?.label ?? offerId;
          card.appendChild(el(doc, "h4", null, title));
          const kind = offer.manifest?.kind ?? "";
          const count = Array.isArray(offer.manifest?.entries) ? offer.manifest.entries.length : 0;
          card.appendChild(el(doc, "p", { class: "offer-meta" }, `${kind} · ${count} file`));
          const ui = local.get(offerId);
          if (ui && (ui.status === "preparing" || ui.status === "withdrawing")) {
            const bar = el(doc, "progress", { max: "100", value: String(Math.round((ui.progress01 ?? 0) * 100)) });
            card.appendChild(bar);
          }
          if (ui && ui.status === "error") {
            card.appendChild(el(doc, "p", { class: "offer-error" }, "Preparazione non riuscita"));
          }
          if (ui && ui.status === "invalid") {
            card.appendChild(el(doc, "p", { class: "offer-error" }, "File modificato: ripubblica"));
          }
          if (ownerId === state.selfPeerId) {
            const withdraw = el(doc, "button", { type: "button", "data-withdraw": offerId }, "Ritira");
            withdraw.addEventListener("click", () => callbacks.onWithdraw(offerId));
            card.appendChild(withdraw);
          }
          // No download path in this phase: no download buttons are created
          // here at all (Phase 3 adds explicit per-offer download on click
          // only).
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
    },
    setStatus,
    /** In-app announcement (status changes, toasts); never the OS API. */
    announce(text) {
      toast.textContent = text;
    },
  };
}
