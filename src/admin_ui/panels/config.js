/**
 * Config panel: server startup configuration (sanitized).
 */

import { escapeHtml } from '../ui.js';

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
                valEl.textContent = 'null';
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
