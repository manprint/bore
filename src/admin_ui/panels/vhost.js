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

            // Header count badge (show total request + response headers)
            const reqCount = vhost.request_headers ? vhost.request_headers.length : 0;
            const respCount = vhost.response_headers ? vhost.response_headers.length : 0;
            const headerCountBadge = badge(`${reqCount} req / ${respCount} resp`, 'info');

            const row = {
                'Subdomain': escapeHtml(vhost.subdomain ?? 'N/A'),
                'Connections': escapeHtml(String(vhost.active ?? 0)),
                'Carriers': escapeHtml(String(vhost.carriers ?? 0)),
                'Direct Opens': escapeHtml(String(vhost.direct_stream_opens ?? 0)),
                'Headers': headerCountBadge,
                'TLS': badgeCell,
                _entry: vhost
            };
            return row;
        });

        const tbl = table(
            ['Subdomain', 'Connections', 'Carriers', 'Direct Opens', 'Headers', 'TLS'],
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
