/**
 * SSH jump-host providers. Credential material and classic usernames are
 * intentionally absent from the API backing this panel.
 */

import { table, notesCell, fmtBytes, fmtDuration, escapeHtml, badgeCell } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

export default {
    id: 'ssh-jump',
    title: 'Jump Hosts',
    route: 'ssh-jump',
    endpoint: '/admin/api/v1/ssh-jump',
    refreshMs: DEFAULT_REFRESH_MS,

    async render(el, data) {
        if (!data || !Array.isArray(data)) {
            el.innerHTML = '<p class="empty-state">No SSH jump-host data</p>';
            return;
        }
        if (data.length === 0) {
            el.innerHTML = '<p class="empty-state">No SSH jump hosts active</p>';
            return;
        }

        const rows = data.map(entry => ({
            'Hostname': escapeHtml(entry.hostname ?? 'N/A'),
            'Port': escapeHtml(String(entry.ssh_port ?? 'N/A')),
            'Provider': badgeCell([
                { label: entry.provider_type === 'ssh' ? 'OpenSSH' : 'Bore', kind: 'secondary' }
            ]),
            'Peer': escapeHtml(entry.peer ?? 'N/A'),
            'Path': badgeCell([
                {
                    label: entry.udp_active
                        ? `Direct (${entry.direct_carriers ?? 0})`
                        : (entry.udp_requested ? 'Relay fallback' : 'Relay'),
                    kind: entry.udp_active ? 'success' : 'secondary'
                }
            ]),
            'Carriers': escapeHtml(`${entry.effective_carriers ?? 0}/${entry.requested_carriers ?? 0}`),
            'Connections': escapeHtml(String(entry.active_connections ?? 0)),
            'Uptime': escapeHtml(fmtDuration(entry.uptime_secs)),
            'TX': escapeHtml(fmtBytes((entry.relay_tx_bytes ?? 0) + (entry.direct_tx_bytes ?? 0))),
            'RX': escapeHtml(fmtBytes((entry.relay_rx_bytes ?? 0) + (entry.direct_rx_bytes ?? 0))),
            'Notes': notesCell(entry.notes, 40),
            _entry: entry
        }));
        const headers = [
            'Hostname', 'Port', 'Provider', 'Peer', 'Path', 'Carriers',
            'Connections', 'Uptime', 'TX', 'RX', 'Notes'
        ];
        const tbl = table(headers, rows);
        const tbody = tbl.querySelector('tbody');
        if (tbody) {
            tbody.querySelectorAll('tr').forEach((tr, index) => {
                tr.style.cursor = 'pointer';
                tr.addEventListener('click', (event) => {
                    if (event.target.closest('.notes-cell')) return;
                    const entry = rows[index]._entry;
                    openModal(`Jump Host ${entry.hostname}`, detailRows(entry));
                });
            });
        }
        el.appendChild(tbl);
    }
};
