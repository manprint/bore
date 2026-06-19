/**
 * Vhost panel: vhost providers table.
 *
 * Structurally identical to the Tunnels panel (same columns/logic): `Subdomain`
 * takes the place of `Port`, flags render through the SHARED `flagBadges` helper,
 * and the row-click detail modal shows every field. Two vhost-only trailing
 * columns are retained — `Direct Opens` and a `Headers` count badge — while the
 * full header pairs / direct pool live in the modal (see
 * docs/frontend/ADMIN_VHOST_PARITY_PLAN.md).
 */

import { table, badge, notesCell, fmtBytes, fmtDuration, escapeHtml, flagBadges, badgeCell } from '../ui.js';
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
            // Header count badge (vhost-only): total request + response headers.
            const reqCount = vhost.request_headers ? vhost.request_headers.length : 0;
            const respCount = vhost.response_headers ? vhost.response_headers.length : 0;
            const headerCountBadge = badge(`${reqCount} req / ${respCount} resp`, 'info');

            const row = {
                'Subdomain': escapeHtml(vhost.subdomain ?? 'N/A'),
                'Peer': escapeHtml(vhost.peer ?? 'N/A'),
                'Flags': badgeCell(flagBadges(vhost)),
                'Connections': escapeHtml(String(vhost.active ?? 0)),
                'Uptime': escapeHtml(fmtDuration(vhost.uptime_secs)),
                'TX': escapeHtml(fmtBytes(vhost.relay_tx_bytes)),
                'RX': escapeHtml(fmtBytes(vhost.relay_rx_bytes)),
                'Notes': notesCell(vhost.notes, 40),
                'Direct Opens': escapeHtml(String(vhost.direct_stream_opens ?? 0)),
                'Headers': headerCountBadge,
                _entry: vhost
            };
            return row;
        });

        const tbl = table(
            ['Subdomain', 'Peer', 'Flags', 'Connections', 'Uptime', 'TX', 'RX', 'Notes', 'Direct Opens', 'Headers'],
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
                    const vhost = rows[idx]._entry;
                    openModal(`Vhost ${vhost.subdomain}`, detailRows(vhost));
                });
            });
        }

        el.appendChild(tbl);
    }
};
