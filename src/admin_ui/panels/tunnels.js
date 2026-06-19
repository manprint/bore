/**
 * Tunnels panel: public tunnels table.
 */

import { table, notesCell, fmtBytes, fmtDuration, escapeHtml, flagBadges, badgeCell } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

/**
 * Flag badges for a public tunnel. Thin alias over the shared `flagBadges`
 * (D5: ONE flag-badge code path across Tunnels/Secret/Vhost). Kept exported for
 * backward compatibility with existing unit tests.
 */
export const tunnelBadges = flagBadges;

export default {
    id: 'tunnels',
    title: 'Tunnels',
    route: 'tunnels',
    endpoint: '/admin/api/v1/tunnels',
    refreshMs: DEFAULT_REFRESH_MS,

    async render(el, data) {
        if (!data || !Array.isArray(data)) {
            el.innerHTML = '<p class="empty-state">No public tunnels</p>';
            return;
        }

        if (data.length === 0) {
            el.innerHTML = '<p class="empty-state">No public tunnels active</p>';
            return;
        }

        const rows = data.map(tunnel => {
            const row = {
                'Port': escapeHtml(String(tunnel.public_port ?? 'N/A')),
                'Peer': escapeHtml(tunnel.peer ?? 'N/A'),
                'Flags': badgeCell(flagBadges(tunnel)),
                'Connections': escapeHtml(String(tunnel.active ?? 0)),
                'Uptime': escapeHtml(fmtDuration(tunnel.uptime_secs)),
                'TX': escapeHtml(fmtBytes(tunnel.relay_tx_bytes)),
                'RX': escapeHtml(fmtBytes(tunnel.relay_rx_bytes)),
                'Notes': notesCell(tunnel.notes, 40),
                _entry: tunnel
            };
            return row;
        });

        const tbl = table(
            ['Port', 'Peer', 'Flags', 'Connections', 'Uptime', 'TX', 'RX', 'Notes'],
            rows
        );

        // Make rows clickable to open detail modal
        const tbody = tbl.querySelector('tbody');
        if (tbody) {
            const trList = tbody.querySelectorAll('tr');
            trList.forEach((tr, idx) => {
                tr.style.cursor = 'pointer';
                tr.addEventListener('click', (e) => {
                    // Don't open modal if click was on a nested expander (notes cell)
                    if (e.target.closest('.notes-cell')) return;
                    const tunnel = rows[idx]._entry;
                    openModal(`Tunnel ${tunnel.public_port}`, detailRows(tunnel));
                });
            });
        }

        el.appendChild(tbl);
    }
};
