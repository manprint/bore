/**
 * T-VPNRENDER: VPN panel render test — verifies path badge, mode, uptime, and flags.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import vpnPanel from '../../src/admin_ui/panels/vpn.js';

test('T-VPNRENDER: vpn panel renders path badge and mode with uptime', async () => {
    const data = {
        links: [
            {
                id: 1,
                role: 'Listener',
                peer: '192.168.1.100:50000',
                overlay: '10.99.0.1/32',
                advertised: ['10.0.0.0/8'],
                carriers: 1,
                direct: true,
                path: 'direct',
                relay_tx_bytes: 1024,
                relay_rx_bytes: 2048,
                uptime_secs: 3600,
                mode: '1:1',
                auto_reconnect: true,
                relay_only: false,
                pin_mtu: false,
                forward_accept: false,
                nat_masquerade: false,
                route_policy: null
            }
        ]
    };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    // Check that the container has the card
    const container = el.children[0];
    assert.ok(container, 'container rendered');

    const card = container.children[0];
    assert.ok(card.classList.contains('vpn-link-card'), 'card has vpn-link-card class');

    // Check header
    const header = card.children[0];
    assert.ok(header.classList.contains('vpn-link-header'), 'header has correct class');

    // Check details section has path and mode
    const details = card.children[1];
    assert.ok(details.classList.contains('vpn-link-details'), 'details section present');

    // Verify innerHTML contains the expected labels
    assert.ok(details._html.includes('Path:'), 'Path label present');
    assert.ok(details._html.includes('Mode:'), 'Mode label present');
    assert.ok(details._html.includes('Uptime:'), 'Uptime label present');
    assert.ok(details._html.includes('Direct'), 'Direct badge present');
    assert.ok(details._html.includes('1:1'), 'mode keyword present');
    assert.ok(details._html.includes('1h'), 'uptime formatting present');
});

test('T-VPNRENDER: vpn panel renders connector flags when set', async () => {
    const data = {
        links: [
            {
                id: 1,
                role: 'Connector',
                peer: '10.0.0.1:50001',
                overlay: '10.99.0.2/32',
                advertised: ['192.168.0.0/24'],
                carriers: 2,
                direct: false,
                path: 'relay',
                relay_tx_bytes: 5000,
                relay_rx_bytes: 10000,
                uptime_secs: 7200,
                mode: '1:1',
                auto_reconnect: true,
                relay_only: true,
                pin_mtu: true,
                mtu: 1350,
                forward_accept: true,
                nat_masquerade: true,
                route_policy: 'accept-all'
            }
        ]
    };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    const card = el.children[0].children[0];
    const details = card.children[1];

    // Check flags are present
    assert.ok(details._html.includes('Flags:'), 'Flags section present when flags are set');
    assert.ok(details._html.includes('Relay-Only'), 'relay_only flag rendered');
    assert.ok(details._html.includes('Pin-MTU'), 'pin_mtu flag rendered');
    assert.ok(details._html.includes('Forward-Accept'), 'forward_accept flag rendered');
    assert.ok(details._html.includes('NAT-Masquerade'), 'nat_masquerade flag rendered');
});

test('T-VPNRENDER: vpn panel omits flags section when all false', async () => {
    const data = {
        links: [
            {
                id: 1,
                role: 'Connector',
                peer: '10.0.0.1:50001',
                overlay: '10.99.0.2/32',
                advertised: [],
                carriers: 1,
                direct: false,
                path: 'relay',
                relay_tx_bytes: 0,
                relay_rx_bytes: 0,
                uptime_secs: 100,
                mode: '1:1',
                auto_reconnect: false,
                relay_only: false,
                pin_mtu: false,
                mtu: null,
                forward_accept: false,
                nat_masquerade: false,
                route_policy: null
            }
        ]
    };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    const card = el.children[0].children[0];
    const details = card.children[1];

    // Flags section should NOT be present when no flags are set
    assert.ok(!details._html.includes('Flags:'), 'Flags section omitted when all false');
});

test('T-VPNRENDER: vpn panel handles hub mode', async () => {
    const data = {
        links: [
            {
                id: 1,
                role: 'Listener',
                peer: '192.168.1.100:50000',
                overlay: '10.99.0.1/32',
                advertised: [],
                carriers: 1,
                direct: true,
                path: 'direct',
                relay_tx_bytes: 0,
                relay_rx_bytes: 0,
                uptime_secs: 50,
                mode: 'hub',
                auto_reconnect: true,
                relay_only: false,
                pin_mtu: false,
                forward_accept: false,
                nat_masquerade: false,
                route_policy: null,
                hub_peers: [
                    {
                        peer_id: 1,
                        overlay: '10.99.0.2/32',
                        peer: '10.0.0.2:50001',
                        advertised: ['10.1.0.0/16']
                    }
                ]
            }
        ]
    };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    const card = el.children[0].children[0];
    const details = card.children[1];

    // Check mode shows "hub"
    assert.ok(details._html.includes('hub'), 'hub mode rendered');
});

test('T-VPNRENDER: vpn panel handles missing path field (fallback to direct bool)', async () => {
    const data = {
        links: [
            {
                id: 1,
                role: 'Connector',
                peer: '10.0.0.1:50001',
                overlay: null,
                advertised: [],
                carriers: 1,
                direct: true,
                // path field is missing — should fall back to direct bool
                relay_tx_bytes: 0,
                relay_rx_bytes: 0,
                uptime_secs: 10,
                mode: '1:1',
                auto_reconnect: false,
                relay_only: false,
                pin_mtu: false,
                forward_accept: false,
                nat_masquerade: false,
                route_policy: null
            }
        ]
    };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    const card = el.children[0].children[0];
    const details = card.children[1];

    // Should render "Direct" even though path is missing
    assert.ok(details._html.includes('Direct'), 'fallback to direct bool works');
});

test('T-VPNRENDER: vpn panel with empty links shows empty state', async () => {
    const data = { links: [] };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    assert.ok(el._html.includes('No VPN links active'), 'empty state message shown');
});
