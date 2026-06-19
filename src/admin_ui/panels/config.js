/**
 * Config panel: server startup configuration (sanitized).
 */

import { escapeHtml } from '../ui.js';

// Byte counts that should read in MiB, to match the sibling udp_*_window values.
const BYTE_KEYS = new Set(['udp_socket_send_buffer', 'udp_socket_recv_buffer']);

/** Format a byte count as a MiB string (e.g. 12582912 → "12 MiB", 13107200 → "12.5 MiB"). */
function fmtMiB(bytes) {
    const mib = bytes / (1024 * 1024);
    const s = Number.isInteger(mib) ? String(mib) : mib.toFixed(2).replace(/\.?0+$/, '');
    return `${s} MiB`;
}

export default {
    id: 'config',
    title: 'Configuration',
    route: 'config',
    endpoint: '/admin/api/v1/config',
    refreshMs: 0, // no polling

    async render(el, data) {
        if (!data || typeof data !== 'object') {
            el.innerHTML = '<p class="empty-state">No configuration data</p>';
            return;
        }

        const container = document.createElement('div');
        container.className = 'config-container';

        const items = Object.entries(data).map(([key, value]) => {
            const row = document.createElement('div');
            row.className = 'config-row';

            const keyEl = document.createElement('div');
            keyEl.className = 'config-key';
            keyEl.textContent = escapeHtml(key);

            const valEl = document.createElement('div');
            valEl.className = 'config-value';

            if (value === null) {
                // Render null with context-specific friendly labels
                if (BYTE_KEYS.has(key)) {
                    valEl.textContent = 'auto (OS default)';
                } else {
                    valEl.textContent = '—';
                }
            } else if (BYTE_KEYS.has(key) && typeof value === 'number') {
                valEl.textContent = escapeHtml(fmtMiB(value));
            } else if (typeof value === 'boolean') {
                valEl.textContent = escapeHtml(value ? 'true' : 'false');
            } else if (typeof value === 'number') {
                valEl.textContent = escapeHtml(String(value));
            } else if (typeof value === 'string') {
                valEl.textContent = escapeHtml(value);
            } else if (Array.isArray(value)) {
                valEl.textContent = escapeHtml(JSON.stringify(value));
            } else if (typeof value === 'object') {
                valEl.textContent = escapeHtml(JSON.stringify(value));
            } else {
                valEl.textContent = escapeHtml(String(value));
            }

            row.appendChild(keyEl);
            row.appendChild(valEl);
            return row;
        });

        items.forEach(item => container.appendChild(item));
        el.appendChild(container);
    }
};
