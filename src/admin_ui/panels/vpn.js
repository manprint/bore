/**
 * VPN panel: one card per VPN link id, grouping the listener and its
 * connector(s) together. Each side shows its own peer, overlay, advertised
 * routes, carriers, notes, NAT UDP port and flags. Hub listeners expand to
 * their peer roster.
 */

import { badge, fmtBytes, fmtDuration, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';
import { openModal, detailRows } from '../modal.js';

/** Build the flag badges shared by both sides. */
function flagBadgesHtml(link) {
    const parts = [];
    if (link.relay_only) parts.push(badge('Relay-Only', 'warning'));
    if (link.pin_mtu) parts.push(badge('Pin-MTU', 'default'));
    if (link.forward_accept) parts.push(badge('Forward-Accept', 'info'));
    if (link.nat_masquerade) parts.push(badge('NAT-Masquerade', 'warning'));
    if (parts.length === 0) return '';
    const html = parts.map(b => b.outerHTML).join(' ');
    return `<div><strong>Flags:</strong> ${html}</div>`;
}

/** Render one side (listener or connector) of a VPN link as a clickable block. */
function renderSide(link) {
    const side = document.createElement('div');
    side.className = 'vpn-side';
    side.style.cursor = 'pointer';

    const pathBadge = link.path === 'direct'
        ? badge('Direct', 'success')
        : badge('Relay', 'warning');

    const advertised = link.advertised && link.advertised.length > 0
        ? escapeHtml(link.advertised.join(', '))
        : 'None';

    const notesHtml = link.notes
        ? `<div><strong>Notes:</strong> ${escapeHtml(link.notes)}</div>`
        : '';

    const natHtml = (link.nat_udp_port !== null && link.nat_udp_port !== undefined)
        ? `<div><strong>NAT UDP port:</strong> ${escapeHtml(String(link.nat_udp_port))}</div>`
        : '';

    side.innerHTML = `
        <div class="vpn-side-head">
            <strong>${escapeHtml(link.role)}</strong> ${pathBadge.outerHTML}
            — Peer: ${escapeHtml(link.peer)}
            | Overlay: ${escapeHtml(link.overlay || 'N/A')}
        </div>
        <div class="vpn-side-body">
            <div><strong>Advertised:</strong> ${advertised}</div>
            <div><strong>Carriers:</strong> ${escapeHtml(String(link.carriers ?? 1))}</div>
            ${notesHtml}
            ${natHtml}
            ${flagBadgesHtml(link)}
            <div><strong>Uptime:</strong> ${escapeHtml(fmtDuration(link.uptime_secs))}</div>
            <div><strong>TX:</strong> ${escapeHtml(fmtBytes(link.relay_tx_bytes))} | <strong>RX:</strong> ${escapeHtml(fmtBytes(link.relay_rx_bytes))}</div>
        </div>
    `;

    side.addEventListener('click', () => {
        openModal(`VPN ${link.role} — ${link.overlay || link.peer}`, detailRows(link));
    });
    return side;
}

/** Render the expandable hub-peer roster (listener side only). */
function renderHubPeers(peers) {
    const section = document.createElement('div');
    section.className = 'vpn-hub-section';

    const toggle = document.createElement('button');
    toggle.className = 'vpn-hub-toggle';
    toggle.textContent = `Hub Peers (${peers.length})`;

    const table = document.createElement('div');
    table.className = 'vpn-hub-peers hidden';
    const list = document.createElement('div');
    list.className = 'vpn-peers-list';
    peers.forEach(peer => {
        const row = document.createElement('div');
        row.className = 'vpn-peer-row';
        row.innerHTML = `
            <span class="vpn-peer-id">#${escapeHtml(String(peer.peer_id))}</span>
            <span class="vpn-peer-overlay">${escapeHtml(peer.overlay)}</span>
            <span class="vpn-peer-addr">${escapeHtml(peer.peer)}</span>
            <span class="vpn-peer-adv">${escapeHtml((peer.advertised || []).join(', ') || 'None')}</span>
        `;
        list.appendChild(row);
    });
    table.appendChild(list);

    let expanded = false;
    toggle.addEventListener('click', () => {
        expanded = !expanded;
        table.classList.toggle('hidden', !expanded);
        toggle.classList.toggle('expanded', expanded);
    });

    section.appendChild(toggle);
    section.appendChild(table);
    return section;
}

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

        // Group listener + connector(s) by the shared link id. Falls back to the
        // per-connection id so a malformed entry still renders on its own.
        const groups = new Map();
        for (const link of links) {
            const key = link.link_id || `#${link.id}`;
            if (!groups.has(key)) {
                groups.set(key, { key, listeners: [], connectors: [] });
            }
            const g = groups.get(key);
            if (link.role === 'vpnlistener') g.listeners.push(link);
            else g.connectors.push(link);
        }

        const container = document.createElement('div');

        for (const group of groups.values()) {
            const sides = [...group.listeners, ...group.connectors];
            const mode = sides.find(s => s.mode)?.mode ?? '1:1';

            const card = document.createElement('div');
            card.className = 'vpn-link-card';

            const header = document.createElement('div');
            header.className = 'vpn-link-header';
            header.innerHTML = `
                <strong>VPN: ${escapeHtml(group.key)}</strong>
                ${badge(mode, 'info').outerHTML}
                <span class="vpn-link-count">${sides.length} endpoint${sides.length === 1 ? '' : 's'}</span>
            `;
            card.appendChild(header);

            const body = document.createElement('div');
            body.className = 'vpn-link-sides';
            if (sides.length === 0) {
                body.innerHTML = '<p class="empty-state">Link registered, no endpoints</p>';
            } else {
                sides.forEach(link => body.appendChild(renderSide(link)));
            }
            card.appendChild(body);

            // Hub peer roster (attached to the listener side by the backend).
            const hubListener = group.listeners.find(l => l.hub_peers && l.hub_peers.length > 0);
            if (hubListener) {
                card.appendChild(renderHubPeers(hubListener.hub_peers));
            }

            container.appendChild(card);
        }

        el.appendChild(container);
    }
};
