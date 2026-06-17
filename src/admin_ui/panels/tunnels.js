/**
 * Tunnels panel: public tunnels table.
 */

import { table, badge, notesCell, fmtBytes, fmtDuration, escapeHtml } from '../ui.js';

export default {
    id: 'tunnels',
    title: 'Tunnels',
    route: 'tunnels',
    endpoint: '/admin/api/v1/tunnels',
    refreshMs: 5000,

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
            const badges = [];
            if (tunnel.https) badges.push(badge('HTTPS', 'primary'));
            if (tunnel.basic_auth) badges.push(badge('Basic Auth', 'warning'));
            if (tunnel.udp) badges.push(badge('UDP', 'success'));

            const badgeCell = document.createElement('span');
            badges.forEach((b, i) => {
                if (i > 0) badgeCell.appendChild(document.createTextNode(' '));
                badgeCell.appendChild(b);
            });

            return {
                'Port': escapeHtml(String(tunnel.public_port ?? 'N/A')),
                'Peer': escapeHtml(tunnel.peer ?? 'N/A'),
                'Flags': badgeCell,
                'Active': escapeHtml(String(tunnel.active ?? 0)),
                'Uptime': escapeHtml(fmtDuration(tunnel.uptime_secs)),
                'TX': escapeHtml(fmtBytes(tunnel.relay_tx_bytes)),
                'RX': escapeHtml(fmtBytes(tunnel.relay_rx_bytes)),
                'Notes': notesCell(tunnel.notes, 40)
            };
        });

        el.appendChild(table(
            ['Port', 'Peer', 'Flags', 'Active', 'Uptime', 'TX', 'RX', 'Notes'],
            rows
        ));
    }
};
