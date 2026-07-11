/**
 * Metrics panel: server uptime, memory, bandwidth, live counts.
 */

import { fmtDuration, fmtBytes, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';

// Rate TX/RX come straight from the server's own 1 s sampler
// (`rate_tx_bps`/`rate_rx_bps`); the old client-side two-sample delta was dropped
// because it depended on the poll cadence + wall clock and showed 0/— when idle.

export default {
    id: 'metrics',
    title: 'Metrics',
    route: 'metrics',
    endpoint: '/admin/api/v1/metrics',
    refreshMs: DEFAULT_REFRESH_MS,

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

        // Total TX/RX are cumulative byte counters from server. Rate now comes
        // from server's own sampler (rate_tx_bps, rate_rx_bps), eliminating the
        // previous client-side delta logic that was fragile on polling issues.
        const cur = {
            tx: data.bandwidth_tx_bytes ?? 0,
            rx: data.bandwidth_rx_bytes ?? 0,
        };

        // Total TX (cumulative)
        const txCard = document.createElement('div');
        txCard.className = 'metric-card';
        txCard.innerHTML = `
            <div class="metric-label">Total TX</div>
            <div class="metric-value">${escapeHtml(fmtBytes(cur.tx))}</div>
        `;
        container.appendChild(txCard);

        // Total RX (cumulative)
        const rxCard = document.createElement('div');
        rxCard.className = 'metric-card';
        rxCard.innerHTML = `
            <div class="metric-label">Total RX</div>
            <div class="metric-value">${escapeHtml(fmtBytes(cur.rx))}</div>
        `;
        container.appendChild(rxCard);

        // Rate TX (from server)
        const rateTxBps = data.rate_tx_bps ?? 0;
        const rateTxCard = document.createElement('div');
        rateTxCard.className = 'metric-card';
        rateTxCard.innerHTML = `
            <div class="metric-label">Rate TX</div>
            <div class="metric-value">${escapeHtml(fmtBytes(rateTxBps))}/s</div>
        `;
        container.appendChild(rateTxCard);

        // Rate RX (from server)
        const rateRxBps = data.rate_rx_bps ?? 0;
        const rateRxCard = document.createElement('div');
        rateRxCard.className = 'metric-card';
        rateRxCard.innerHTML = `
            <div class="metric-label">Rate RX</div>
            <div class="metric-value">${escapeHtml(fmtBytes(rateRxBps))}/s</div>
        `;
        container.appendChild(rateRxCard);

        // Live counts section
        const countsSection = document.createElement('div');
        countsSection.className = 'metrics-counts';

        const countsCard = document.createElement('div');
        countsCard.className = 'card';

        const countsList = document.createElement('div');
        countsList.className = 'counts-list';

        // Public tunnels
        const publicCount = document.createElement('div');
        publicCount.className = 'count-row';
        publicCount.innerHTML = `
            <span class="count-label">Public Tunnels</span>
            <span class="count-value">${escapeHtml(String(data.public_tunnels ?? 0))}</span>
        `;
        countsList.appendChild(publicCount);

        // Secret tunnels
        const secretCount = document.createElement('div');
        secretCount.className = 'count-row';
        secretCount.innerHTML = `
            <span class="count-label">Secret Tunnels</span>
            <span class="count-value">${escapeHtml(String(data.secret_tunnels ?? 0))}</span>
        `;
        countsList.appendChild(secretCount);

        // Vhost domains
        const vhostCount = document.createElement('div');
        vhostCount.className = 'count-row';
        vhostCount.innerHTML = `
            <span class="count-label">Vhost Domains</span>
            <span class="count-value">${escapeHtml(String(data.vhost_domains ?? 0))}</span>
        `;
        countsList.appendChild(vhostCount);

        // SSH Tunnels (if present)
        if (data.ssh_tunnels !== undefined) {
            const sshCount = document.createElement('div');
            sshCount.className = 'count-row';
            sshCount.innerHTML = `
                <span class="count-label">SSH Tunnels</span>
                <span class="count-value">${escapeHtml(String(data.ssh_tunnels ?? 0))}</span>
            `;
            countsList.appendChild(sshCount);
        }

        // Active Connections
        if (data.active_connections !== undefined) {
            const activeConn = document.createElement('div');
            activeConn.className = 'count-row';
            activeConn.innerHTML = `
                <span class="count-label">Active Connections</span>
                <span class="count-value">${escapeHtml(String(data.active_connections ?? 0))}</span>
            `;
            countsList.appendChild(activeConn);
        }

        // Auth Failures
        if (data.auth_failures !== undefined) {
            const authFail = document.createElement('div');
            authFail.className = 'count-row';
            authFail.innerHTML = `
                <span class="count-label">Auth Failures</span>
                <span class="count-value">${escapeHtml(String(data.auth_failures ?? 0))}</span>
            `;
            countsList.appendChild(authFail);
        }

        // Conn Rejections
        if (data.conn_rejections !== undefined) {
            const connRej = document.createElement('div');
            connRej.className = 'count-row';
            connRej.innerHTML = `
                <span class="count-label">Conn Rejections</span>
                <span class="count-value">${escapeHtml(String(data.conn_rejections ?? 0))}</span>
            `;
            countsList.appendChild(connRej);
        }

        // Direct Fallbacks
        if (data.direct_fallbacks !== undefined) {
            const directFb = document.createElement('div');
            directFb.className = 'count-row';
            directFb.innerHTML = `
                <span class="count-label">Direct Fallbacks</span>
                <span class="count-value">${escapeHtml(String(data.direct_fallbacks ?? 0))}</span>
            `;
            countsList.appendChild(directFb);
        }

        // Transport breakdown
        if (data.transport_bore !== undefined || data.transport_ssh !== undefined) {
            const transportLabel = document.createElement('div');
            transportLabel.className = 'count-label' ;
            transportLabel.textContent = 'Transport Breakdown';
            countsList.appendChild(transportLabel);

            if (data.transport_bore !== undefined) {
                const boreTrans = document.createElement('div');
                boreTrans.className = 'count-row';
                boreTrans.innerHTML = `
                    <span class="count-label" style="padding-left: 1.5rem;">Bore</span>
                    <span class="count-value">${escapeHtml(String(data.transport_bore ?? 0))}</span>
                `;
                countsList.appendChild(boreTrans);
            }

            if (data.transport_ssh !== undefined) {
                const sshTrans = document.createElement('div');
                sshTrans.className = 'count-row';
                sshTrans.innerHTML = `
                    <span class="count-label" style="padding-left: 1.5rem;">SSH</span>
                    <span class="count-value">${escapeHtml(String(data.transport_ssh ?? 0))}</span>
                `;
                countsList.appendChild(sshTrans);
            }
        }

        countsCard.appendChild(countsList);
        countsSection.appendChild(countsCard);
        container.appendChild(countsSection);

        el.appendChild(container);
    }
};
