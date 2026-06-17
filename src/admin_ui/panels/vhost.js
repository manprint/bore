/**
 * Vhost panel: vhost providers table.
 */

import { table, badge, escapeHtml } from '../ui.js';

export default {
    id: 'vhost',
    title: 'Vhost',
    route: 'vhost',
    endpoint: '/admin/api/v1/vhost',
    refreshMs: 5000,

    async render(el, data) {
        if (!data || !Array.isArray(data)) {
            el.innerHTML = '<p class="empty-state">No vhost providers</p>';
            return;
        }

        if (data.length === 0) {
            el.innerHTML = '<p class="empty-state">No vhost providers active</p>';
            return;
        }

        const rows = data.map(vhost => {
            const badges = [];
            if (vhost.tls) badges.push(badge('TLS', 'primary'));

            const badgeCell = document.createElement('span');
            badges.forEach((b, i) => {
                if (i > 0) badgeCell.appendChild(document.createTextNode(' '));
                badgeCell.appendChild(b);
            });

            const reqHeaders = vhost.request_headers
                ? escapeHtml(vhost.request_headers.join(', '))
                : 'None';
            const respHeaders = vhost.response_headers
                ? escapeHtml(vhost.response_headers.join(', '))
                : 'None';

            return {
                'Subdomain': escapeHtml(vhost.subdomain ?? 'N/A'),
                'Active': escapeHtml(String(vhost.active ?? 0)),
                'Carriers': escapeHtml(String(vhost.carriers ?? 0)),
                'Direct Opens': escapeHtml(String(vhost.direct_stream_opens ?? 0)),
                'Request Headers': reqHeaders,
                'Response Headers': respHeaders,
                'TLS': badgeCell
            };
        });

        el.appendChild(table(
            ['Subdomain', 'Active', 'Carriers', 'Direct Opens', 'Request Headers', 'Response Headers', 'TLS'],
            rows
        ));
    }
};
