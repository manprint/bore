/**
 * Token store: get/set/clear token in sessionStorage.
 * Never log the token; never put it in URLs.
 */

const TOKEN_KEY = 'bore_admin_token';
const REFRESH_INTERVAL_KEY = 'bore_refresh_interval';

export function getToken() {
    return sessionStorage.getItem(TOKEN_KEY);
}

export function setToken(token) {
    sessionStorage.setItem(TOKEN_KEY, token);
}

export function clearToken() {
    sessionStorage.removeItem(TOKEN_KEY);
}

export function getRefreshInterval() {
    return parseInt(sessionStorage.getItem(REFRESH_INTERVAL_KEY) || '5000', 10);
}

export function setRefreshInterval(ms) {
    sessionStorage.setItem(REFRESH_INTERVAL_KEY, String(ms));
}
