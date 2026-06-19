/**
 * Reusable modal component for showing detail views.
 *
 * openModal(title, rows): Display a modal with a list of label→value pairs.
 * rows: array of { label, value } objects.
 *
 * Modal features:
 * - Appends overlay to document.body (not inside #view, so polling re-render doesn't destroy it)
 * - Close on X click, Esc keydown, or overlay click-outside
 * - Only one modal open at a time (re-opening closes the previous one)
 *
 * detailRows(obj): Convert an object to { label, value } array, applying:
 *   - Keys ending `_bytes` → fmtBytes
 *   - Keys ending `_secs` → fmtDuration
 *   - Booleans → badge('Yes'/'No')
 *   - Arrays → joined with ', '
 *   - null/undefined → '—'
 *   - Other → escaped string
 */

import { fmtBytes, fmtDuration, badge, escapeHtml } from './ui.js';

let _currentOverlay = null;

/**
 * Close the currently open modal, if any.
 */
export function closeModal() {
    if (_currentOverlay) {
        // Handle both real DOM and test stub
        if (_currentOverlay.parentNode) {
            _currentOverlay.parentNode.removeChild(_currentOverlay);
        } else if (document.body && document.body.children) {
            // Test stub: removeChild from body directly
            const idx = document.body.children.indexOf(_currentOverlay);
            if (idx >= 0) {
                document.body.children.splice(idx, 1);
            }
        }
    }
    _currentOverlay = null;
    document.removeEventListener('keydown', _onEscapeKey);
}

/**
 * Handle Esc keypress to close the modal.
 */
function _onEscapeKey(e) {
    if (e.key === 'Escape') {
        closeModal();
    }
}

/**
 * Open a detail modal with the given title and rows.
 * rows: [ { label: string, value: any }, ... ]
 */
export function openModal(title, rows) {
    // Close any existing modal first
    closeModal();

    // Create overlay (click-outside to close)
    const overlay = document.createElement('div');
    overlay.className = 'modal-overlay';
    overlay.addEventListener('click', (e) => {
        // Only close if click is on the overlay itself, not the modal content
        if (e.target === overlay) {
            closeModal();
        }
    });

    // Create modal box
    const modal = document.createElement('div');
    modal.className = 'modal';

    // Header with title and close button
    const header = document.createElement('div');
    header.className = 'modal-header';

    const titleEl = document.createElement('h2');
    titleEl.className = 'modal-title';
    titleEl.textContent = escapeHtml(title);
    header.appendChild(titleEl);

    const closeBtn = document.createElement('button');
    closeBtn.className = 'modal-close';
    closeBtn.textContent = '×';
    closeBtn.addEventListener('click', closeModal);
    header.appendChild(closeBtn);

    modal.appendChild(header);

    // Body: render rows as a <dl> or table
    const body = document.createElement('div');
    body.className = 'modal-body';

    const dl = document.createElement('dl');
    rows.forEach(row => {
        const dt = document.createElement('dt');
        dt.textContent = escapeHtml(row.label);
        dl.appendChild(dt);

        const dd = document.createElement('dd');
        if (row.value instanceof HTMLElement) {
            dd.appendChild(row.value);
        } else {
            dd.textContent = escapeHtml(String(row.value ?? '—'));
        }
        dl.appendChild(dd);
    });
    body.appendChild(dl);

    modal.appendChild(body);
    overlay.appendChild(modal);

    // Append to body and track the overlay
    document.body.appendChild(overlay);
    _currentOverlay = overlay;

    // Listen for Esc to close
    document.addEventListener('keydown', _onEscapeKey);
}

/**
 * Convert an object to a detailRows array, applying smart formatting.
 * Keys ending `_bytes` → fmtBytes; `_secs` → fmtDuration; etc.
 */
export function detailRows(obj) {
    const rows = [];
    for (const [key, value] of Object.entries(obj)) {
        // Skip internal metadata fields (those starting with _)
        if (key.startsWith('_')) continue;

        let label = key
            .replace(/_/g, ' ')
            .replace(/\b\w/g, (c) => c.toUpperCase());

        let formatted;
        if (value === null || value === undefined) {
            formatted = '—';
        } else if (key.endsWith('_bytes')) {
            formatted = fmtBytes(value);
        } else if (key.endsWith('_secs')) {
            formatted = fmtDuration(value);
        } else if (typeof value === 'boolean') {
            formatted = badge(value ? 'Yes' : 'No', value ? 'success' : 'default');
        } else if (Array.isArray(value)) {
            // Special case: [[k,v], [k,v], ...] header pairs → render as key: value
            if (value.length > 0 && Array.isArray(value[0]) && value[0].length === 2) {
                formatted = value.map(([k, v]) => `${escapeHtml(k)}: ${escapeHtml(v)}`).join('; ');
            } else {
                // Regular array → join with ', '
                formatted = value.map(v => escapeHtml(String(v))).join(', ');
            }
        } else {
            formatted = escapeHtml(String(value));
        }

        rows.push({ label, value: formatted });
    }
    return rows;
}
