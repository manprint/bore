/**
 * Certs panel: TLS certificate expiry tracking.
 */

import { badge, fmtDate, escapeHtml } from '../ui.js';

export default {
    id: 'certs',
    title: 'Certificates',
    route: 'certs',
    endpoint: '/admin/api/v1/certs',
    refreshMs: 60000,

    async render(el, data) {
        if (!data || !Array.isArray(data)) {
            el.innerHTML = '<p class="empty-state">No certificates</p>';
            return;
        }

        if (data.length === 0) {
            el.innerHTML = '<p class="empty-state">No certificates configured</p>';
            return;
        }

        const container = document.createElement('div');

        data.forEach(cert => {
            const card = document.createElement('div');
            card.className = 'cert-card';

            // Color badge based on expiry status
            let statusKind = 'success';
            let statusText = 'Valid';
            if (cert.error) {
                statusKind = 'danger';
                statusText = 'Error';
            } else if (cert.days_remaining < 0) {
                statusKind = 'danger';
                statusText = 'Expired';
            } else if (cert.days_remaining <= 30) {
                statusKind = 'warning';
                statusText = 'Expiring';
            }

            const statusBadge = badge(statusText, statusKind);

            const header = document.createElement('div');
            header.className = 'cert-header';
            header.innerHTML = `
                <strong>${escapeHtml(cert.label)}</strong>
                — ${statusBadge.outerHTML}
            `;

            const details = document.createElement('div');
            details.className = 'cert-details';

            if (cert.error) {
                details.innerHTML = `<div class="error-message">${escapeHtml(cert.error)}</div>`;
            } else {
                const subject = cert.subject ? escapeHtml(cert.subject) : 'N/A';
                const sans = cert.sans && cert.sans.length > 0
                    ? escapeHtml(cert.sans.join(', '))
                    : 'None';
                const notAfter = cert.not_after ? escapeHtml(fmtDate(cert.not_after)) : 'N/A';
                const daysRemaining = cert.days_remaining !== undefined
                    ? escapeHtml(String(cert.days_remaining))
                    : 'N/A';

                details.innerHTML = `
                    <div><strong>Subject:</strong> ${subject}</div>
                    <div><strong>SANs:</strong> ${sans}</div>
                    <div><strong>Expires:</strong> ${notAfter}</div>
                    <div><strong>Days Remaining:</strong> ${daysRemaining}</div>
                `;
            }

            card.appendChild(header);
            card.appendChild(details);
            container.appendChild(card);
        });

        el.appendChild(container);
    }
};
