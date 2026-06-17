/**
 * Metrics panel: server uptime, memory, bandwidth, and live counts.
 */

import { fmtDuration, fmtBytes, escapeHtml } from '../ui.js';

export default {
    id: 'metrics',
    title: 'Metrics',
    route: 'metrics',
    endpoint: '/admin/api/v1/metrics',
    refreshMs: 3000,

    async render(el, data) {
        if (!data || typeof data !== 'object') {
            el.innerHTML = '<p class="empty-state">No metrics data</p>';
            return;
        }

        const container = document.createElement('div');
        container.className = 'metrics-container';

        // Uptime
        const uptimeCard = document.createElement('div');
        uptimeCard.className = 'metric-card';
        uptimeCard.innerHTML = `
            <div class="metric-label">Uptime</div>
            <div class="metric-value">${escapeHtml(fmtDuration(data.uptime_secs))}</div>
        `;
        container.appendChild(uptimeCard);

        // Memory RSS
        const memCard = document.createElement('div');
        memCard.className = 'metric-card';
        const memValue = data.mem_rss_bytes !== null && data.mem_rss_bytes !== undefined
            ? escapeHtml(fmtBytes(data.mem_rss_bytes))
            : 'N/A (non-Linux)';
        memCard.innerHTML = `
            <div class="metric-label">Memory RSS</div>
            <div class="metric-value">${memValue}</div>
        `;
        container.appendChild(memCard);

        // Bandwidth TX
        const txCard = document.createElement('div');
        txCard.className = 'metric-card';
        txCard.innerHTML = `
            <div class="metric-label">Bandwidth TX</div>
            <div class="metric-value">${escapeHtml(fmtBytes(data.bandwidth_tx_bytes))}</div>
        `;
        container.appendChild(txCard);

        // Bandwidth RX
        const rxCard = document.createElement('div');
        rxCard.className = 'metric-card';
        rxCard.innerHTML = `
            <div class="metric-label">Bandwidth RX</div>
            <div class="metric-value">${escapeHtml(fmtBytes(data.bandwidth_rx_bytes))}</div>
        `;
        container.appendChild(rxCard);

        // Live counts section
        const countsSection = document.createElement('div');
        countsSection.className = 'metrics-counts';

        const countsCard = document.createElement('div');
        countsCard.className = 'card';

        const countsList = document.createElement('div');
        countsList.className = 'counts-list';

        const tunnelsCount = document.createElement('div');
        tunnelsCount.className = 'count-row';
        tunnelsCount.innerHTML = `
            <span><strong>Live Tunnels:</strong></span>
            <span>${escapeHtml(String(data.live_tunnels ?? 0))}</span>
        `;
        countsList.appendChild(tunnelsCount);

        const vhostCount = document.createElement('div');
        vhostCount.className = 'count-row';
        vhostCount.innerHTML = `
            <span><strong>Live Vhost:</strong></span>
            <span>${escapeHtml(String(data.live_vhost ?? 0))}</span>
        `;
        countsList.appendChild(vhostCount);

        if (data.live_vpn_links !== undefined) {
            const vpnCount = document.createElement('div');
            vpnCount.className = 'count-row';
            vpnCount.innerHTML = `
                <span><strong>Live VPN Links:</strong></span>
                <span>${escapeHtml(String(data.live_vpn_links))}</span>
            `;
            countsList.appendChild(vpnCount);
        }

        countsCard.appendChild(countsList);
        countsSection.appendChild(countsCard);
        container.appendChild(countsSection);

        el.appendChild(container);
    }
};
