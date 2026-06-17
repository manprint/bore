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
    cell.className = 'notes-cell';

    if (!text || text.length <= maxLen) {
        cell.textContent = escapeHtml(text || '');
        return cell;
    }

    const truncated = escapeHtml(text.substring(0, maxLen)) + '...';
    cell.innerHTML = truncated;

    cell.addEventListener('click', (e) => {
        e.preventDefault();
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
