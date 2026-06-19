/**
 * Secret panel: secret tunnels table.
 */

import { table, badge, notesCell, fmtBytes, fmtDuration, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

export default {
    id: 'secret',
    title: 'Secret',
    route: 'secret',
    endpoint: '/admin/api/v1/secret',
    refreshMs: DEFAULT_REFRESH_MS,

    async render(el, data) {
        if (!data || !Array.isArray(data)) {
            el.innerHTML = '<p class="empty-state">No secret tunnels</p>';
            return;
        }

        if (data.length === 0) {
            el.innerHTML = '<p class="empty-state">No secret tunnels active</p>';
            return;
        }

        const rows = data.map(secret => {
            const badges = [];
            if (secret.udp) badges.push(badge('UDP', 'success'));
            if (secret.basic_auth) badges.push(badge('Basic Auth', 'warning'));
            if (secret.carriers > 1) badges.push(badge(`x${secret.carriers} carriers`, 'default'));

            const badgeCell = document.createElement('span');
            badges.forEach((b, i) => {
                if (i > 0) badgeCell.appendChild(document.createTextNode(' '));
                badgeCell.appendChild(b);
            });

            const row = {
                'Role': escapeHtml(secret.role ?? 'N/A'),
                'Secret ID': escapeHtml(secret.secret_id ?? 'N/A'),
                'Peer': escapeHtml(secret.peer ?? 'N/A'),
                'Flags': badgeCell,
                'Active': escapeHtml(String(secret.active ?? 0)),
                'Uptime': escapeHtml(fmtDuration(secret.uptime_secs)),
                'TX': escapeHtml(fmtBytes(secret.relay_tx_bytes)),
                'RX': escapeHtml(fmtBytes(secret.relay_rx_bytes)),
                _entry: secret
            };
            return row;
        });

        const tbl = table(
            ['Role', 'Secret ID', 'Peer', 'Flags', 'Active', 'Uptime', 'TX', 'RX'],
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
                    const secret = rows[idx]._entry;
                    openModal(`Secret ${secret.secret_id}`, detailRows(secret));
                });
            });
        }

        el.appendChild(tbl);
    }
};
