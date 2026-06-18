/**
 * Hash router: listen to #/<route> and dispatch to the appropriate panel.
 */

// Set by setupRouter; re-renders the currently active route. Used by the poll
// timer (BUG-0) so auto-refresh actually re-fetches + repaints the live panel.
let _refresh = null;

export function setupRouter(registry) {
    function getRoute() {
        const hash = window.location.hash.slice(1); // Remove #
        if (hash.startsWith('/')) {
            return hash.slice(1); // Remove leading /
        }
        return '';
    }

    function findPanel(route) {
        if (!route) return registry[0]; // Default to first
        return registry.find(p => p.route === route) || registry[0];
    }

    async function renderPanel(route) {
        const panel = findPanel(route);
        const view = document.getElementById('view');
        view.innerHTML = ''; // Clear

        // Update sidebar active state
        document.querySelectorAll('#menu a').forEach(a => {
            a.classList.remove('active');
            if (a.getAttribute('href') === `#/${panel.route}`) {
                a.classList.add('active');
            }
        });

        try {
            const ctx = { route, panel };
            // Fetch data if there's an endpoint
            if (panel.endpoint) {
                const { apiGet } = await import('./api.js');
                const data = await apiGet(panel.endpoint);
                await panel.render(view, data, ctx);
            } else {
                await panel.render(view, null, ctx);
            }
        } catch (err) {
            view.innerHTML = `<p style="color:red">Error loading panel: ${err.message}</p>`;
        }
    }

    // Expose a refresh hook for the poll timer (re-fetch + repaint current route).
    _refresh = () => renderPanel(getRoute());

    // Handle hash change
    window.addEventListener('hashchange', () => {
        renderPanel(getRoute());
    });

    // Initial render
    renderPanel(getRoute());
}

/**
 * Re-fetch and re-render the currently active route. No-op until setupRouter has
 * run. This is what the polling timer calls every `refreshMs`.
 */
export function refreshCurrent() {
    if (_refresh) return _refresh();
}
