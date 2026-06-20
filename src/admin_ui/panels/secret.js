/**
 * Secret panel: secret tunnels grouped by secret_id into cards.
 * Each card shows providers and consumers, mirroring the VPN panel structure.
 */

import { badge, fmtBytes, fmtDuration, escapeHtml, flagBadges, badgeCell } from '../ui.js';
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

        // Group by secret_id; null/missing → fallback to #<id>
        const groups = new Map();
        for (const entry of data) {
            const key = entry.secret_id || `#${entry.id}`;
            if (!groups.has(key)) {
                groups.set(key, { key, providers: [], consumers: [] });
            }
            const g = groups.get(key);
            if (entry.role === 'secretprovider') g.providers.push(entry);
            else if (entry.role === 'secretconsumer') g.consumers.push(entry);
        }

        const container = document.createElement('div');

        for (const group of groups.values()) {
            const card = document.createElement('div');
            card.className = 'secret-card';

            // Card header
            const header = document.createElement('div');
            header.className = 'secret-card-header';
            header.innerHTML = `<strong>Secret: ${escapeHtml(group.key)}</strong>`;
            card.appendChild(header);

            // Card body
            const body = document.createElement('div');
            body.className = 'secret-card-body';

            // Render providers
            if (group.providers.length > 0) {
                const provSection = document.createElement('div');
                provSection.className = 'secret-role-section';
                const provLabel = document.createElement('div');
                provLabel.className = 'secret-role-label';
                provLabel.innerHTML = `<strong>Provider${group.providers.length > 1 ? 's' : ''}</strong>`;
                provSection.appendChild(provLabel);

                const provTable = renderEntryTable(group.providers);
                provSection.appendChild(provTable);
                body.appendChild(provSection);
            }

            // Render consumers
            if (group.consumers.length > 0) {
                const consSection = document.createElement('div');
                consSection.className = 'secret-role-section';
                const consLabel = document.createElement('div');
                consLabel.className = 'secret-role-label';
                consLabel.innerHTML = `<strong>Consumer${group.consumers.length > 1 ? 's' : ''}</strong>`;
                consSection.appendChild(consLabel);

                const consTable = renderEntryTable(group.consumers);
                consSection.appendChild(consTable);
                body.appendChild(consSection);
            }

            card.appendChild(body);
            container.appendChild(card);
        }

        el.appendChild(container);
    }
};

/**
 * Render a table of entries with role-specific columns.
 */
function renderEntryTable(entries) {
    const table = document.createElement('table');
    table.className = 'secret-entry-table';

    const thead = document.createElement('thead');
    const headerRow = document.createElement('tr');
    ['Peer', 'Local', 'Flags', 'Connections', 'Uptime', 'TX', 'RX', 'Notes'].forEach(h => {
        const th = document.createElement('th');
        th.textContent = escapeHtml(h);
        headerRow.appendChild(th);
    });
    thead.appendChild(headerRow);
    table.appendChild(thead);

    const tbody = document.createElement('tbody');
    entries.forEach(entry => {
        const tr = document.createElement('tr');
        tr.style.cursor = 'pointer';

        // Peer
        const peerCell = document.createElement('td');
        peerCell.textContent = escapeHtml(entry.peer ?? 'N/A');
        tr.appendChild(peerCell);

        // Local (provider: local_host:local_port, consumer: local_proxy_port)
        const localCell = document.createElement('td');
        if (entry.role === 'secretprovider') {
            if (entry.local_host && entry.local_port) {
                localCell.textContent = escapeHtml(`${entry.local_host}:${entry.local_port}`);
            } else {
                localCell.textContent = 'N/A';
            }
        } else if (entry.role === 'secretconsumer') {
            if (entry.local_proxy_port) {
                localCell.textContent = escapeHtml(String(entry.local_proxy_port));
            } else {
                localCell.textContent = 'N/A';
            }
        }
        tr.appendChild(localCell);

        // Flags
        const flagsCell = document.createElement('td');
        flagsCell.appendChild(badgeCell(flagBadges(entry)));
        tr.appendChild(flagsCell);

        // Connections
        const connCell = document.createElement('td');
        connCell.textContent = escapeHtml(String(entry.active ?? 0));
        tr.appendChild(connCell);

        // Uptime
        const uptimeCell = document.createElement('td');
        uptimeCell.textContent = escapeHtml(fmtDuration(entry.uptime_secs));
        tr.appendChild(uptimeCell);

        // TX
        const txCell = document.createElement('td');
        txCell.textContent = escapeHtml(fmtBytes(entry.relay_tx_bytes));
        tr.appendChild(txCell);

        // RX
        const rxCell = document.createElement('td');
        rxCell.textContent = escapeHtml(fmtBytes(entry.relay_rx_bytes));
        tr.appendChild(rxCell);

        // Notes
        const notesCell = document.createElement('td');
        notesCell.textContent = escapeHtml(entry.notes ?? '—');
        tr.appendChild(notesCell);

        // Click handler
        tr.addEventListener('click', () => {
            openModal(`Secret ${entry.secret_id || 'N/A'} — ${entry.peer}`, detailRows(entry));
        });

        tbody.appendChild(tr);
    });
    table.appendChild(tbody);

    return table;
}
