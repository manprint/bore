/**
 * Config panel: server startup configuration (sanitized).
 */

import { badge, escapeHtml } from '../ui.js';

// Byte counts should read in MiB, match sibling udp_*_window values.
const BYTE_KEYS = new Set(['udp_socket_send_buffer', 'udp_socket_recv_buffer']);

// Merged vhost configuration (phase 06.4): header maps and the reservation
// list are structured values, so the generic String(value) row would render
// them as "[object Object]".
const HEADER_MAP_KEYS = new Set([
    'vhost_default_request_headers',
    'vhost_default_response_headers',
]);
const RESERVATIONS_KEY = 'vhost_reservations';

// Pretty labels for SSH and other config keys
const PRETTY_LABELS = {
    'server_version': 'Server Version',
    'vhost_default_request_headers': 'Vhost Default Request Headers',
    'vhost_default_response_headers': 'Vhost Default Response Headers',
    'vhost_reservations': 'Vhost Reservations',
    'ssh_gateway': 'SSH Gateway',
    'ssh_jump_enabled': 'Jump Hosts Enabled',
    'ssh_jump_base_domain': 'Jump Base Domain',
    'ssh_jump_classic_auth_required': 'Jump Classic Auth Required',
    'ssh_jump_direct_quic_port': 'Jump Direct QUIC Port',
    'ssh_port': 'SSH Port',
    'ssh_advertise_address': 'Advertise Address',
    'ssh_advertise_port': 'Advertise Port',
    'ssh_auth_pubkey': 'Public-Key Auth',
    'ssh_auth_password': 'Password Auth',
    'ssh_banner': 'Banner',
    'ssh_host_key_file': 'Host Key File',
};

/** Format byte count MiB string (e.g. 12582912 → "12 MiB", 13107200 → "12.5 MiB"). */
function fmtMiB(bytes) {
    const mib = bytes / (1024 * 1024);
    const s = Number.isInteger(mib) ? String(mib) : mib.toFixed(2).replace(/\.?0+$/, '');
    return `${s} MiB`;
}

/**
 * Render a header map ({name: value}) as one "name: value" line per entry.
 * Empty map reads as "none" — an operator must be able to tell "no headers
 * configured" from "the endpoint does not report them" (F-6).
 */
function renderHeaderMap(valEl, map) {
    const names = Object.keys(map).sort();
    if (names.length === 0) {
        valEl.textContent = 'none';
        return;
    }
    valEl.className = 'config-value config-value-stack';
    for (const name of names) {
        const line = document.createElement('div');
        line.className = 'config-subvalue';
        line.textContent = escapeHtml(`${name}: ${map[name]}`);
        valEl.appendChild(line);
    }
}

/**
 * Render the static subdomain reservations, one line each:
 * "app → team-a (1 req / 2 resp headers)".
 */
function renderReservations(valEl, list) {
    if (!Array.isArray(list) || list.length === 0) {
        valEl.textContent = 'none';
        return;
    }
    valEl.className = 'config-value config-value-stack';
    for (const res of list) {
        const req = res.headers ? Object.keys(res.headers).length : 0;
        const resp = res.response_headers ? Object.keys(res.response_headers).length : 0;
        const line = document.createElement('div');
        line.className = 'config-subvalue';
        line.textContent = escapeHtml(
            `${res.subdomain} \u2192 ${res.client_id} (${req} req / ${resp} resp headers)`
        );
        valEl.appendChild(line);
    }
}

/**
 * Check if a key is part of the SSH Gateway group.
 */
function isSshKey(key) {
    return key.startsWith('ssh_');
}

export default {
    id: 'config',
    title: 'Configuration',
    route: 'config',
    endpoint: '/admin/api/v1/config',
    refreshMs: 0, // no polling

    async render(el, data) {
        if (!data || typeof data !== 'object') {
            el.innerHTML = '<p class="empty-state">No configuration data</p>';
            return;
        }

        const container = document.createElement('div');
        container.className = 'config-container';

        // Separate SSH keys from the rest
        const sshKeys = [];
        const otherKeys = [];

        for (const key of Object.keys(data).sort()) {
            if (isSshKey(key)) {
                sshKeys.push(key);
            } else {
                otherKeys.push(key);
            }
        }

        // Render non-SSH keys first
        for (const key of otherKeys) {
            const value = data[key];
            const row = document.createElement('div');
            row.className = 'config-row';

            const keyEl = document.createElement('div');
            keyEl.className = 'config-key';
            keyEl.textContent = escapeHtml(PRETTY_LABELS[key] || key);

            const valEl = document.createElement('div');
            valEl.className = 'config-value';

            if (HEADER_MAP_KEYS.has(key)) {
                renderHeaderMap(valEl, value || {});
            } else if (key === RESERVATIONS_KEY) {
                renderReservations(valEl, value);
            } else if (value === null) {
                if (BYTE_KEYS.has(key)) {
                    valEl.textContent = 'auto (OS default)';
                } else {
                    valEl.textContent = '—';
                }
            } else if (BYTE_KEYS.has(key) && typeof value === 'number') {
                valEl.textContent = escapeHtml(fmtMiB(value));
            } else if (typeof value === 'boolean') {
                valEl.appendChild(badge(value ? 'Yes' : 'No', value ? 'success' : 'default'));
            } else {
                valEl.textContent = escapeHtml(String(value));
            }

            row.appendChild(keyEl);
            row.appendChild(valEl);
            container.appendChild(row);
        }

        // Add SSH Gateway header if needed
        if (sshKeys.length > 0) {
            const sshHeader = document.createElement('div');
            sshHeader.className = 'config-header';
            sshHeader.textContent = 'SSH Gateway';
            container.appendChild(sshHeader);
        }

        // Render SSH keys
        for (const key of sshKeys) {
            const value = data[key];
            const row = document.createElement('div');
            row.className = 'config-row';

            const keyEl = document.createElement('div');
            keyEl.className = 'config-key';
            keyEl.textContent = escapeHtml(PRETTY_LABELS[key] || key);

            const valEl = document.createElement('div');
            valEl.className = 'config-value';

            if (value === null) {
                valEl.textContent = '—';
            } else if (typeof value === 'boolean') {
                valEl.appendChild(badge(value ? 'Yes' : 'No', value ? 'success' : 'default'));
            } else {
                valEl.textContent = escapeHtml(String(value));
            }

            row.appendChild(keyEl);
            row.appendChild(valEl);
            container.appendChild(row);
        }

        el.appendChild(container);
    }
};
