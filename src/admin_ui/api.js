/**
 * API helper: fetch with authorization header.
 * On 401, clear token and dispatch unauthorized event.
 */

import { getToken, clearToken } from './store.js';

export async function apiGet(endpoint) {
    const token = getToken();
    const headers = {};
    if (token) {
        headers['Authorization'] = `Bearer ${token}`;
    }

    // `no-store`: each poll must hit the server, never a cached response
    // (the server also sends Cache-Control: no-store; this covers intermediaries).
    const response = await fetch(endpoint, { headers, cache: 'no-store' });

    if (response.status === 401) {
        clearToken();
        document.dispatchEvent(new CustomEvent('bore:unauthorized'));
        throw new Error('Unauthorized');
    }

    if (!response.ok) {
        throw new Error(`HTTP ${response.status}: ${response.statusText}`);
    }

    return await response.json();
}
