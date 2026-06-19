/**
 * T-OVR: Overview panel test — verifies it reads the new field names.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import overviewPanel from '../../src/admin_ui/panels/overview.js';

test('T-OVR: overview renders the new summary field names', async () => {
    const data = {
        version: '0.5.0',
        control_port: 7835,
        uptime_secs: 3600,
        public_tunnels: 1,
        secret_tunnels: 2,
        vhost_domains: 1,
        vpn_enabled: true,
        vpn_links: 3,
        tls: true,
        udp: true,
        vhost_enabled: true,
    };

    const el = document.createElement('div');
    await overviewPanel.render(el, data);

    // Check that the card grid has the expected cards (innerHTML contains the field names)
    const grid = el.children[0];
    assert.ok(grid.classList.contains('card-grid'), 'card grid rendered');

    // HTML should contain the new field names (even though we can't parse it with the stub,
    // the _html property demonstrates that the right labels are being rendered)
    assert.ok(grid.children.length >= 6, 'at least 6 card divs present');

    // Check that innerHTML contains the actual field labels (this is a proxy test since the stub
    // doesn't parse innerHTML)
    let foundPublic = false, foundSecret = false, foundVhost = false, foundVpn = false;
    for (const card of grid.children) {
        const html = card._html || '';
        if (html.includes('Public Tunnels')) foundPublic = true;
        if (html.includes('Secret Tunnels')) foundSecret = true;
        if (html.includes('Vhost')) foundVhost = true;
        if (html.includes('VPN Links')) foundVpn = true;
    }
    assert.ok(foundPublic && foundSecret && foundVhost && foundVpn, 'all expected field names present in rendered HTML');

    // Check features flag section
    const flagsCard = el.children[1];
    assert.ok(flagsCard._html.includes('TLS'), 'TLS flag present');
    assert.ok(flagsCard._html.includes('UDP'), 'UDP flag present');
    assert.ok(flagsCard._html.includes('Vhost'), 'Vhost flag present');
    assert.ok(flagsCard._html.includes('VPN'), 'VPN flag present');
});

test('T-OVR: overview hides vpn_links card when vpn_enabled is false', async () => {
    const data = {
        version: '0.5.0',
        control_port: 7835,
        uptime_secs: 3600,
        public_tunnels: 1,
        secret_tunnels: 0,
        vhost_domains: 0,
        vpn_enabled: false,
        tls: true,
        udp: false,
        vhost_enabled: false,
    };

    const el = document.createElement('div');
    await overviewPanel.render(el, data);

    const grid = el.children[0];
    const cards = [];
    for (const child of grid.children) {
        if (child.classList && child.classList.contains('card-item')) {
            cards.push(child);
        }
    }
    const vpnCard = cards.find(c => c.children && c.children[0] && c.children[0].textContent === 'VPN Links');
    assert.ok(!vpnCard, 'VPN Links card not rendered when vpn_enabled is false');
});