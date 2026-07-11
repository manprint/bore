/**
 * Config panel: server startup configuration (sanitized).
 */

import { badge, escapeHtml } from '../ui.js';

// Byte counts should read in MiB, match sibling udp_*_window values.
const BYTE_KEYS = new Set(['udp_socket_send_buffer', 'udp_socket_recv_buffer']);

// Pretty labels for SSH and other config keys
const PRETTY_LABELS = {
    'ssh_gateway': 'SSH Gateway',
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

            if (value === null) {
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
