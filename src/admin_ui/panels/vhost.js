/**
 * Vhost panel: vhost providers table.
 */

import { table, badge, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

export default {
    id: 'vhost',
    title: 'Vhost',
    route: 'vhost',
    endpoint: '/admin/api/v1/vhost',
    refreshMs: DEFAULT_REFRESH_MS,

    async render(el, data) {
        if (!data || !Array.isArray(data)) {
            el.innerHTML = '<p class="empty-state">No vhost providers</p>';
            return;
        }

        if (data.length === 0) {
            el.innerHTML = '<p class="empty-state">No vhost providers active</p>';
            return;
        }

        const rows = data.map(vhost => {
            const badges = [];
            if (vhost.tls) badges.push(badge('TLS', 'primary'));

            const badgeCell = document.createElement('span');
            badges.forEach((b, i) => {
                if (i > 0) badgeCell.appendChild(document.createTextNode(' '));
                badgeCell.appendChild(b);
            });

            const reqHeaders = vhost.request_headers
                ? escapeHtml(vhost.request_headers.join(', '))
                : 'None';
            const respHeaders = vhost.response_headers
                ? escapeHtml(vhost.response_headers.join(', '))
                : 'None';

            const row = {
                'Subdomain': escapeHtml(vhost.subdomain ?? 'N/A'),
                'Active': escapeHtml(String(vhost.active ?? 0)),
                'Carriers': escapeHtml(String(vhost.carriers ?? 0)),
                'Direct Opens': escapeHtml(String(vhost.direct_stream_opens ?? 0)),
                'Request Headers': reqHeaders,
                'Response Headers': respHeaders,
                'TLS': badgeCell,
                _entry: vhost
            };
            return row;
        });

        const tbl = table(
            ['Subdomain', 'Active', 'Carriers', 'Direct Opens', 'Request Headers', 'Response Headers', 'TLS'],
            rows
        );

        // Make rows clickable to open detail modal
        const tbody = tbl.querySelector('tbody');
        if (tbody) {
            const trList = tbody.querySelectorAll('tr');
            trList.forEach((tr, idx) => {
                tr.style.cursor = 'pointer';
                tr.addEventListener('click', (e) => {
                    if (e.target.closest('.notes-cell')) return;
                    const vhost = rows[idx]._entry;
                    openModal(`Vhost ${vhost.subdomain}`, detailRows(vhost));
                });
            });
        }

        el.appendChild(tbl);
    }
};
