/**
 * Hash router: listen to #/<route> and dispatch to the appropriate panel.
 */

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

    // Handle hash change
    window.addEventListener('hashchange', () => {
        renderPanel(getRoute());
    });

    // Initial render
    renderPanel(getRoute());
}
