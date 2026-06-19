/**
 * Metrics panel: server uptime, memory, bandwidth, and live counts.
 */

import { fmtDuration, fmtBytes, escapeHtml } from '../ui.js';
import { DEFAULT_REFRESH_MS } from '../poller.js';

/**
 * BUG-5: the `bandwidth_*` fields are CUMULATIVE totals, not a rate. Derive a
 * rate from two successive samples: bytes/second per direction. Returns null on
 * the first sample or a non-positive time delta (avoids NaN/Infinity).
 * `prev`/`cur` = { tx, rx, t } where t is seconds.
 */
export function rateFromSamples(prev, cur) {
    if (!prev || !cur || cur.t <= prev.t) return null;
    const dt = cur.t - prev.t;
    return {
        txbps: Math.max(0, (cur.tx - prev.tx) / dt),
        rxbps: Math.max(0, (cur.rx - prev.rx) / dt),
    };
}

// Module-scoped last sample; persists across polls so the rate is delta-based.
let _lastSample = null;

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

        // Total TX/RX are cumulative byte counters; the rate is derived from the
        // delta between successive polls (BUG-5: these were mislabeled "Bandwidth"
        // and never updated because polling was broken — see poller.js / BUG-0).
        const now = Date.now() / 1000;
        const cur = {
            tx: data.bandwidth_tx_bytes ?? 0,
            rx: data.bandwidth_rx_bytes ?? 0,
            t: now,
        };
        const rate = rateFromSamples(_lastSample, cur);
        _lastSample = cur;
        const rateStr = (bps) => (rate ? `${fmtBytes(bps)}/s` : '—');

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

        // Rate TX (derived)
        const rateTxCard = document.createElement('div');
        rateTxCard.className = 'metric-card';
        rateTxCard.innerHTML = `
            <div class="metric-label">Rate TX</div>
            <div class="metric-value">${escapeHtml(rateStr(rate && rate.txbps))}</div>
        `;
        container.appendChild(rateTxCard);

        // Rate RX (derived)
        const rateRxCard = document.createElement('div');
        rateRxCard.className = 'metric-card';
        rateRxCard.innerHTML = `
            <div class="metric-label">Rate RX</div>
            <div class="metric-value">${escapeHtml(rateStr(rate && rate.rxbps))}</div>
        `;
        container.appendChild(rateRxCard);

        // Live counts section
        const countsSection = document.createElement('div');
        countsSection.className = 'metrics-counts';

        const countsCard = document.createElement('div');
        countsCard.className = 'card';

        const countsList = document.createElement('div');
        countsList.className = 'counts-list';

        const publicCount = document.createElement('div');
        publicCount.className = 'count-row';
        publicCount.innerHTML = `
            <span><strong>Public Tunnels:</strong></span>
            <span>${escapeHtml(String(data.public_tunnels ?? 0))}</span>
        `;
        countsList.appendChild(publicCount);

        const secretCount = document.createElement('div');
        secretCount.className = 'count-row';
        secretCount.innerHTML = `
            <span><strong>Secret Tunnels:</strong></span>
            <span>${escapeHtml(String(data.secret_tunnels ?? 0))}</span>
        `;
        countsList.appendChild(secretCount);

        const vhostCount = document.createElement('div');
        vhostCount.className = 'count-row';
        vhostCount.innerHTML = `
            <span><strong>Vhost Domains:</strong></span>
            <span>${escapeHtml(String(data.vhost_domains ?? 0))}</span>
        `;
        countsList.appendChild(vhostCount);

        if (data.vpn_links !== undefined) {
            const vpnCount = document.createElement('div');
            vpnCount.className = 'count-row';
            vpnCount.innerHTML = `
                <span><strong>VPN Links:</strong></span>
                <span>${escapeHtml(String(data.vpn_links))}</span>
            `;
            countsList.appendChild(vpnCount);
        }

        countsCard.appendChild(countsList);
        countsSection.appendChild(countsCard);
        container.appendChild(countsSection);

        el.appendChild(container);
    }
};
