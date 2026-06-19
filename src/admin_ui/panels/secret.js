/**
 * Secret panel: secret tunnels table.
 */

import { table, notesCell, fmtBytes, fmtDuration, escapeHtml, flagBadges, badgeCell } from '../ui.js';
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
            const row = {
                'Role': escapeHtml(secret.role ?? 'N/A'),
                'Secret ID': escapeHtml(secret.secret_id ?? 'N/A'),
                'Peer': escapeHtml(secret.peer ?? 'N/A'),
                'Flags': badgeCell(flagBadges(secret)),
                'Connections': escapeHtml(String(secret.active ?? 0)),
                'Uptime': escapeHtml(fmtDuration(secret.uptime_secs)),
                'TX': escapeHtml(fmtBytes(secret.relay_tx_bytes)),
                'RX': escapeHtml(fmtBytes(secret.relay_rx_bytes)),
                'Notes': notesCell(secret.notes, 40),
                _entry: secret
            };
            return row;
        });

        const tbl = table(
            ['Role', 'Secret ID', 'Peer', 'Flags', 'Connections', 'Uptime', 'TX', 'RX', 'Notes'],
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
