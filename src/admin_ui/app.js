/**
 * Bootstrap: build sidebar from registry, init router, wire login.
 * Panel-agnostic; imports only registry, router, store, api.
 */

import registry from './registry.js';
import { setupRouter } from './router.js';
import { getToken, setToken, clearToken } from './store.js';

let pollInterval = null;

function buildSidebar() {
    const menu = document.getElementById('menu');
    menu.innerHTML = '';

    registry.forEach(panel => {
        const a = document.createElement('a');
        a.href = `#/${panel.route}`;
        a.textContent = panel.title;
        menu.appendChild(a);
    });
}

function setupLoginOverlay() {
    const overlay = document.getElementById('login-overlay');
    const form = document.getElementById('login-form');
    const input = document.getElementById('token-input');

    if (!getToken()) {
        overlay.classList.remove('hidden');
    }

    form.addEventListener('submit', (e) => {
        e.preventDefault();
        const token = input.value.trim();
        if (token) {
            setToken(token);
            input.value = '';
            overlay.classList.add('hidden');
            // Re-render current panel with auth
            window.location.hash = window.location.hash || '#/overview';
        }
    });

    // Listen for 401 unauthorized events
    document.addEventListener('bore:unauthorized', () => {
        clearToken();
        input.value = '';
        overlay.classList.remove('hidden');
    });
}

function setupPolling() {
    const activePanel = registry.find(p => p.route === (window.location.hash.slice(1).split('/')[1] || 'overview'));
    if (activePanel && activePanel.refreshMs > 0) {
        if (pollInterval) clearInterval(pollInterval);
        pollInterval = setInterval(() => {
            // Re-fetch and re-render
            window.dispatchEvent(new CustomEvent('panel:refresh'));
        }, activePanel.refreshMs);
    } else {
        if (pollInterval) clearInterval(pollInterval);
    }
}

window.addEventListener('hashchange', setupPolling);

buildSidebar();
setupLoginOverlay();
setupRouter(registry);
setupPolling();
