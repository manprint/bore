/**
 * VPN panel: VPN links with hub peer expansion.
 */

import { badge, fmtBytes, escapeHtml } from '../ui.js';

export default {
    id: 'vpn',
    title: 'VPN',
    route: 'vpn',
    endpoint: '/admin/api/v1/vpn',
    refreshMs: 5000,

    async render(el, data) {
        // VPN endpoint returns { links: [...] }
        const links = data?.links ?? [];

        if (links.length === 0) {
            el.innerHTML = '<p class="empty-state">No VPN links active</p>';
            return;
        }

        const container = document.createElement('div');

        links.forEach(link => {
            const card = document.createElement('div');
            card.className = 'vpn-link-card';

            const badges = [];
            if (link.direct) {
                badges.push(badge('Direct', 'success'));
            } else {
                badges.push(badge('Relay', 'warning'));
            }

            const badgeCell = document.createElement('span');
            badges.forEach((b, i) => {
                if (i > 0) badgeCell.appendChild(document.createTextNode(' '));
                badgeCell.appendChild(b);
            });

            const advertised = link.advertised && link.advertised.length > 0
                ? escapeHtml(link.advertised.join(', '))
                : 'None';

            const header = document.createElement('div');
            header.className = 'vpn-link-header';
            header.innerHTML = `
                <strong>${escapeHtml(link.role)}</strong>
                — Peer: ${escapeHtml(link.peer)}
                | Overlay: ${escapeHtml(link.overlay || 'N/A')}
            `;

            const details = document.createElement('div');
            details.className = 'vpn-link-details';
            details.innerHTML = `
                <div><strong>Advertised:</strong> ${advertised}</div>
                <div><strong>Carriers:</strong> ${escapeHtml(String(link.carriers ?? 1))}</div>
                <div><strong>Direct/Relay:</strong> ${badgeCell.innerHTML}</div>
                <div><strong>TX:</strong> ${escapeHtml(fmtBytes(link.relay_tx_bytes))} | <strong>RX:</strong> ${escapeHtml(fmtBytes(link.relay_rx_bytes))}</div>
            `;

            card.appendChild(header);
            card.appendChild(details);

            // If hub peers exist, render expandable sub-table
            if (link.hub_peers && link.hub_peers.length > 0) {
                const peersSection = document.createElement('div');
                peersSection.className = 'vpn-hub-section';

                const toggle = document.createElement('button');
                toggle.className = 'vpn-hub-toggle';
                toggle.textContent = `Hub Peers (${link.hub_peers.length})`;
                let expanded = false;

                const peersTable = document.createElement('div');
                peersTable.className = 'vpn-hub-peers hidden';

                // Render hub peers as a compact list/table
                const peersList = document.createElement('div');
                peersList.className = 'vpn-peers-list';
                link.hub_peers.forEach(peer => {
                    const peerRow = document.createElement('div');
                    peerRow.className = 'vpn-peer-row';
                    peerRow.innerHTML = `
                        <span class="vpn-peer-id">#${escapeHtml(String(peer.peer_id))}</span>
                        <span class="vpn-peer-overlay">${escapeHtml(peer.overlay)}</span>
                        <span class="vpn-peer-addr">${escapeHtml(peer.peer)}</span>
                        <span class="vpn-peer-adv">${escapeHtml((peer.advertised || []).join(', ') || 'None')}</span>
                    `;
                    peersList.appendChild(peerRow);
                });
                peersTable.appendChild(peersList);

                toggle.addEventListener('click', () => {
                    expanded = !expanded;
                    if (expanded) {
                        peersTable.classList.remove('hidden');
                        toggle.classList.add('expanded');
                    } else {
                        peersTable.classList.add('hidden');
                        toggle.classList.remove('expanded');
                    }
                });

                peersSection.appendChild(toggle);
                peersSection.appendChild(peersTable);
                card.appendChild(peersSection);
            }

            container.appendChild(card);
        });

        el.appendChild(container);
    }
};
