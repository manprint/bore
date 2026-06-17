/**
 * Panel registry: imports each panel and exports an ordered array.
 * This is THE ONLY file that imports panel modules (I-6).
 *
 * Panel contract:
 * {
 *   id: string (unique identifier),
 *   title: string (displayed in sidebar),
 *   route: string (hash route, e.g., 'tunnels'),
 *   endpoint: string (API endpoint, or null if no data fetch),
 *   refreshMs: number (polling interval, 0 = no polling),
 *   render(el, data, ctx): void (async render function)
 * }
 */

import overview from './panels/overview.js';
import tunnels from './panels/tunnels.js';
import secret from './panels/secret.js';
import vhost from './panels/vhost.js';
import vpn from './panels/vpn.js';
import certs from './panels/certs.js';
import config from './panels/config.js';
import metrics from './panels/metrics.js';

export default [
    overview,
    tunnels,
    secret,
    vhost,
    vpn,
    certs,
    config,
    metrics,
];
