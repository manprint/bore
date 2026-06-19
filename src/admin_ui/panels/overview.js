/**
 * Overview panel: summary view with version, ports, counts.
 */

import { fmtDuration, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';

export default {
    id: 'overview',
    title: 'Overview',
    route: 'overview',
    endpoint: '/admin/api/v1/summary',
    refreshMs: DEFAULT_REFRESH_MS,

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

        // Listeners & Ports card
        if (data.port_range || data.bind_tunnels || (data.vhost_enabled && (data.vhost_http_port || data.vhost_https_port || data.vhost_quic_port))) {
            const ports = document.createElement('div');
            ports.className = 'card';
            let portContent = '<strong>Listeners & Ports:</strong><br>';
            portContent += `Control: ${escapeHtml(String(data.control_port || 'N/A'))}<br>`;
            if (data.port_range) {
                portContent += `Port Range: ${escapeHtml(data.port_range)}<br>`;
            }
            if (data.bind_tunnels) {
                portContent += `Tunnel Bind: ${escapeHtml(data.bind_tunnels)}<br>`;
            }
            if (data.vhost_enabled) {
                if (data.vhost_http_port) {
                    portContent += `Vhost HTTP: ${escapeHtml(String(data.vhost_http_port))}<br>`;
                }
                if (data.vhost_https_port) {
                    portContent += `Vhost HTTPS: ${escapeHtml(String(data.vhost_https_port))}<br>`;
                }
                if (data.vhost_quic_port) {
                    portContent += `Vhost QUIC: ${escapeHtml(String(data.vhost_quic_port))}<br>`;
                }
            }
            ports.innerHTML = portContent;
            el.appendChild(ports);
        }
    }
};
