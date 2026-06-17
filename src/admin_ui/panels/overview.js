/**
 * Overview panel: summary view with version, ports, counts.
 */

import { fmtDuration, escapeHtml } from '../ui.js';

export default {
    id: 'overview',
    title: 'Overview',
    route: 'overview',
    endpoint: '/admin/api/v1/summary',
    refreshMs: 5000,

    async render(el, data) {
        if (!data) {
            el.innerHTML = '<p>No data</p>';
            return;
        }

        const grid = document.createElement('div');
        grid.className = 'card-grid';

        const cards = [
            { label: 'Version', value: escapeHtml(data.version || 'N/A') },
            { label: 'Control Port', value: escapeHtml(String(data.control_port || 'N/A')) },
            { label: 'Uptime', value: escapeHtml(fmtDuration(data.uptime_secs)) },
            { label: 'Public Tunnels', value: escapeHtml(String(data.public_tunnels || 0)) },
            { label: 'Secret Tunnels', value: escapeHtml(String(data.secret_tunnels || 0)) },
            { label: 'Vhost', value: escapeHtml(String(data.vhost_domains || 0)) },
        ];

        if (data.vpn_enabled) {
            cards.push({ label: 'VPN Links', value: escapeHtml(String(data.vpn_links || 0)) });
        }

        cards.forEach(c => {
            const card = document.createElement('div');
            card.className = 'card-item';
            card.innerHTML = `
                <div class="card-item-label">${escapeHtml(c.label)}</div>
                <div class="card-item-value">${c.value}</div>
            `;
            grid.appendChild(card);
        });

        const flags = document.createElement('div');
        flags.className = 'card';
        const flagText = [
            data.tls ? 'TLS' : '',
            data.udp ? 'UDP' : '',
            data.vhost_enabled ? 'Vhost' : '',
            data.vpn_enabled ? 'VPN' : '',
        ]
            .filter(Boolean)
            .join(', ') || 'None';
        flags.innerHTML = `<strong>Features:</strong> ${escapeHtml(flagText)}`;

        el.appendChild(grid);
        el.appendChild(flags);
    }
};
