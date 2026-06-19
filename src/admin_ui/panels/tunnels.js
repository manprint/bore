/**
 * Tunnels panel: public tunnels table.
 */

import { table, badge, notesCell, fmtBytes, fmtDuration, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

/**
 * Flag badges for a public tunnel (BUG-3). Pure (no DOM) so it is unit-testable;
 * the render maps each spec through `badge()`. Covers every operator-visible
 * flag: https, force_https, basic_auth, udp, carriers (>1), auto_reconnect.
 */
export function tunnelBadges(t) {
    const b = [];
    if (t.https) b.push({ label: 'HTTPS', kind: 'primary' });
    if (t.force_https) b.push({ label: 'Force-HTTPS', kind: 'primary' });
    if (t.basic_auth) b.push({ label: 'Basic Auth', kind: 'warning' });
    if (t.udp) b.push({ label: 'UDP', kind: 'success' });
    if (t.carriers > 1) b.push({ label: `x${t.carriers} carriers`, kind: 'default' });
    if (t.auto_reconnect) b.push({ label: 'Auto-reconnect', kind: 'success' });
    return b;
}

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
            const badgeCell = document.createElement('span');
            tunnelBadges(tunnel).forEach((spec, i) => {
                if (i > 0) badgeCell.appendChild(document.createTextNode(' '));
                badgeCell.appendChild(badge(spec.label, spec.kind));
            });

            const row = {
                'Port': escapeHtml(String(tunnel.public_port ?? 'N/A')),
                'Peer': escapeHtml(tunnel.peer ?? 'N/A'),
                'Flags': badgeCell,
                'Active': escapeHtml(String(tunnel.active ?? 0)),
                'Uptime': escapeHtml(fmtDuration(tunnel.uptime_secs)),
                'TX': escapeHtml(fmtBytes(tunnel.relay_tx_bytes)),
                'RX': escapeHtml(fmtBytes(tunnel.relay_rx_bytes)),
                'Notes': notesCell(tunnel.notes, 40),
                _entry: tunnel
            };
            return row;
        });

        const tbl = table(
            ['Port', 'Peer', 'Flags', 'Active', 'Uptime', 'TX', 'RX', 'Notes'],
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
