/**
 * VPN panel: VPN links with hub peer expansion.
 */

import { badge, fmtBytes, fmtDuration, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

export default {
    id: 'vpn',
    title: 'VPN',
    route: 'vpn',
    endpoint: '/admin/api/v1/vpn',
    refreshMs: DEFAULT_REFRESH_MS,

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

            // Path badge: direct or relay
            const pathBadges = [];
            const pathStr = link.path ?? (link.direct ? 'direct' : 'relay');
            if (pathStr === 'direct') {
                pathBadges.push(badge('Direct', 'success'));
            } else {
                pathBadges.push(badge('Relay', 'warning'));
            }

            const pathBadgeCell = document.createElement('span');
            pathBadges.forEach((b, i) => {
                if (i > 0) pathBadgeCell.appendChild(document.createTextNode(' '));
                pathBadgeCell.appendChild(b);
            });

            // Mode badge (1:1 or hub)
            const modeStr = link.mode ?? '1:1';
            const modeBadge = badge(modeStr, 'info');

            // Flag badges for card display (show only if truthy)
            const flagBadges = [];
            if (link.relay_only) flagBadges.push(badge('Relay-Only', 'warning'));
            if (link.pin_mtu) flagBadges.push(badge('Pin-MTU', 'default'));
            if (link.forward_accept) flagBadges.push(badge('Forward-Accept', 'info'));
            if (link.nat_masquerade) flagBadges.push(badge('NAT-Masquerade', 'warning'));

            const advertised = link.advertised && link.advertised.length > 0
                ? escapeHtml(link.advertised.join(', '))
                : 'None';

            const header = document.createElement('div');
            header.className = 'vpn-link-header';
            header.style.cursor = 'pointer';
            header.innerHTML = `
                <strong>${escapeHtml(link.role)}</strong>
                — Peer: ${escapeHtml(link.peer)}
                | Overlay: ${escapeHtml(link.overlay || 'N/A')}
            `;
            header.addEventListener('click', (e) => {
                // Don't open modal if click is on the toggle button
                if (e.target.closest('.vpn-hub-toggle')) return;
                openModal(`VPN Link ${link.overlay || link.peer}`, detailRows(link));
            });

            const details = document.createElement('div');
            details.className = 'vpn-link-details';

            // Build flag badges HTML
            let flagsDisplay = '';
            if (flagBadges.length > 0) {
                const flagHtmlParts = flagBadges.map(b => {
                    const kind = b.className?.split('badge-')[1] || 'default';
                    return `<span class="badge badge-${escapeHtml(kind)}">${escapeHtml(b.textContent)}</span>`;
                });
                flagsDisplay = `<div><strong>Flags:</strong> ${flagHtmlParts.join(' ')}</div>`;
            }

            // Build mode badge HTML
            const modeHtml = `<span class="badge badge-info">${escapeHtml(modeStr)}</span>`;

            details.innerHTML = `
                <div><strong>Path:</strong> ${pathBadgeCell.innerHTML}</div>
                <div><strong>Mode:</strong> ${modeHtml}</div>
                <div><strong>Uptime:</strong> ${escapeHtml(fmtDuration(link.uptime_secs))}</div>
                <div><strong>Advertised:</strong> ${advertised}</div>
                <div><strong>Carriers:</strong> ${escapeHtml(String(link.carriers ?? 1))}</div>
                ${flagsDisplay}
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
