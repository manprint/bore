/**
 * UI helpers: HTML escaping, formatting, component builders.
 * CRITICAL: escapeHtml is applied to EVERY rendered user-controlled value.
 */

/**
 * Escape HTML special characters: &<>"'
 */
export function escapeHtml(text) {
    if (text == null) return '';
    return String(text)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

/**
 * Format bytes as human-readable.
 */
export function fmtBytes(bytes) {
    if (bytes == null || bytes < 0) return 'N/A';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    let size = bytes;
    let unitIdx = 0;
    while (size >= 1024 && unitIdx < units.length - 1) {
        size /= 1024;
        unitIdx++;
    }
    return `${size.toFixed(2)} ${units[unitIdx]}`;
}

/**
 * Format seconds as human-readable duration.
 */
export function fmtDuration(secs) {
    if (secs == null || secs < 0) return 'N/A';
    const d = Math.floor(secs / 86400);
    const h = Math.floor((secs % 86400) / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = Math.floor(secs % 60);
    const parts = [];
    if (d > 0) parts.push(`${d}d`);
    if (h > 0) parts.push(`${h}h`);
    if (m > 0) parts.push(`${m}m`);
    if (s > 0 || parts.length === 0) parts.push(`${s}s`);
    return parts.join(' ');
}

/**
 * Parse RFC3339 date and format as readable string.
 */
export function fmtDate(rfc3339) {
    if (!rfc3339) return 'N/A';
    try {
        const d = new Date(rfc3339);
        return d.toLocaleDateString() + ' ' + d.toLocaleTimeString();
    } catch {
        return escapeHtml(rfc3339);
    }
}

/**
 * Render a badge element.
 */
export function badge(text, kind = 'default') {
    const span = document.createElement('span');
    span.className = `badge badge-${kind}`;
    span.textContent = escapeHtml(text);
    return span;
}

/**
 * Build the flag-badge specs for ANY live-tunnel entry (public, secret, vhost).
 * Pure (no DOM) so it is unit-testable; callers map specs through `badgeCell`.
 *
 * This is the SINGLE source of flag badges across the Tunnels/Secret/Vhost
 * sections (consistency by construction): each section passes its entry and gets
 * the same labels/kinds for the same flags. A flag absent from an entry simply
 * yields no badge. Both `https` (public) and `tls` (vhost) map to a TLS-class
 * badge so the concept reads identically across sections.
 *
 * Returns: [{ label: string, kind: string }, ...] using only CSS-defined kinds.
 */
export function flagBadges(e) {
    const b = [];
    if (e.https) b.push({ label: 'HTTPS', kind: 'primary' });
    if (e.force_https) b.push({ label: 'Force-HTTPS', kind: 'primary' });
    if (e.tls) b.push({ label: 'TLS', kind: 'primary' });
    if (e.basic_auth) b.push({ label: 'Basic Auth', kind: 'warning' });
    if (e.udp) b.push({ label: 'UDP', kind: 'success' });
    if (e.carriers > 1) b.push({ label: `x${e.carriers} carriers`, kind: 'default' });
    if (e.auto_reconnect) b.push({ label: 'Auto-reconnect', kind: 'success' });
    if (e.webserver_log) b.push({ label: 'Weblog', kind: 'default' });
    if (e.upnp === true) b.push({ label: 'UPnP', kind: 'default' });
    if (e.try_port_prediction === true) b.push({ label: 'Port-Pred', kind: 'default' });
    if (e.nat_udp_preferred_port > 0) b.push({ label: `NAT:${e.nat_udp_preferred_port}`, kind: 'default' });
    return b;
}

/**
 * Render an array of badge specs ({label, kind}) into a single span of
 * space-separated badges (used for the "Flags" column in every section).
 */
export function badgeCell(specs) {
    const cell = document.createElement('span');
    specs.forEach((spec, i) => {
        if (i > 0) cell.appendChild(document.createTextNode(' '));
        cell.appendChild(badge(spec.label, spec.kind));
    });
    return cell;
}

/**
 * Render a table from headers and rows.
 * rows: array of objects with keys matching headers.
 */
export function table(headers, rows) {
    const tbl = document.createElement('table');
    const thead = document.createElement('thead');
    const tr = document.createElement('tr');
    headers.forEach(h => {
        const th = document.createElement('th');
        th.textContent = escapeHtml(h);
        tr.appendChild(th);
    });
    thead.appendChild(tr);
    tbl.appendChild(thead);

    const tbody = document.createElement('tbody');
    rows.forEach(row => {
        const tr = document.createElement('tr');
        headers.forEach(h => {
            const td = document.createElement('td');
            const val = row[h];
            if (val instanceof HTMLElement) {
                td.appendChild(val);
            } else {
                td.textContent = escapeHtml(String(val ?? ''));
            }
            tr.appendChild(td);
        });
        tbody.appendChild(tr);
    });
    tbl.appendChild(tbody);
    return tbl;
}

/**
 * Render a notes cell with click-to-expand behavior.
 * Truncates to maxLen characters, shows "..." and expands on click.
 */
export function notesCell(text, maxLen = 50) {
    const cell = document.createElement('span');

    // BUG-2: only notes long enough to be truncated get the clickable
    // (underlined, pointer) affordance + expand handler. Short/empty notes are
    // plain text — previously they carried the clickable class with no handler,
    // so they looked like a link but did nothing.
    if (!text || text.length <= maxLen) {
        cell.className = 'notes-plain';
        cell.textContent = text || ''; // textContent is auto-escaped by the DOM
        return cell;
    }

    cell.className = 'notes-cell';
    cell.setAttribute('title', text);
    const truncated = escapeHtml(text.substring(0, maxLen)) + '...';
    cell.innerHTML = truncated;

    cell.addEventListener('click', () => {
        if (cell.classList.contains('expanded')) {
            cell.classList.remove('expanded');
            cell.innerHTML = truncated;
        } else {
            cell.classList.add('expanded');
            cell.innerHTML = escapeHtml(text);
        }
    });

    return cell;
}
