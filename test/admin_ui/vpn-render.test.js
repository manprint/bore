/**
 * T-VPNRENDER: VPN panel render test — verifies the grouped card structure
 * (header with mode badge + per-side details), path/flags/uptime, and hub roster.
 *
 * The panel groups listener + connector(s) by link id; each side is a
 * `.vpn-side` block inside `.vpn-link-sides`, and the mode badge lives in the
 * `.vpn-link-header`. Role grouping keys on the backend's lowercase role
 * strings ("vpnlistener"/"vpnconnector").
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import vpnPanel from '../../src/admin_ui/panels/vpn.js';

function cardOf(el) {
    const container = el.children[0];
    assert.ok(container, 'container rendered');
    const card = container.children[0];
    assert.ok(card.classList.contains('vpn-link-card'), 'card has vpn-link-card class');
    return card;
}

test('T-VPNRENDER: vpn panel renders path badge and mode with uptime', async () => {
    const data = {
        links: [
            {
                id: 1,
                link_id: 'site-a',
                role: 'vpnlistener',
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

    const card = cardOf(el);

    // Header carries the link id + the mode badge.
    const header = card.children[0];
    assert.ok(header.classList.contains('vpn-link-header'), 'header has correct class');
    assert.ok(header.innerHTML.includes('site-a'), 'link id in header');
    assert.ok(header.innerHTML.includes('1:1'), 'mode badge in header');

    // The side block carries path/uptime.
    const body = card.children[1];
    assert.ok(body.classList.contains('vpn-link-sides'), 'sides container present');
    const side = body.children[0];
    assert.ok(side.classList.contains('vpn-side'), 'side block present');
    assert.ok(side.innerHTML.includes('Direct'), 'Direct path badge present');
    assert.ok(side.innerHTML.includes('Uptime:'), 'Uptime label present');
    assert.ok(side.innerHTML.includes('1h'), 'uptime formatting present');
});

test('T-VPNRENDER: vpn panel renders connector flags when set', async () => {
    const data = {
        links: [
            {
                id: 1,
                link_id: 'site-a',
                role: 'vpnconnector',
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

    const side = cardOf(el).children[1].children[0];
    assert.ok(side.innerHTML.includes('Flags:'), 'Flags section present when flags are set');
    assert.ok(side.innerHTML.includes('Relay-Only'), 'relay_only flag rendered');
    assert.ok(side.innerHTML.includes('Pin-MTU'), 'pin_mtu flag rendered');
    assert.ok(side.innerHTML.includes('Forward-Accept'), 'forward_accept flag rendered');
    assert.ok(side.innerHTML.includes('NAT-Masquerade'), 'nat_masquerade flag rendered');
});

test('T-VPNRENDER: vpn panel omits flags section when all false', async () => {
    const data = {
        links: [
            {
                id: 1,
                link_id: 'site-a',
                role: 'vpnconnector',
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

    const side = cardOf(el).children[1].children[0];
    assert.ok(!side.innerHTML.includes('Flags:'), 'Flags section omitted when all false');
});

test('T-VPNRENDER: vpn panel handles hub mode', async () => {
    const data = {
        links: [
            {
                id: 1,
                link_id: 'hub-a',
                role: 'vpnlistener',
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

    const card = cardOf(el);
    // Mode badge shows "hub" in the header.
    assert.ok(card.children[0].innerHTML.includes('hub'), 'hub mode rendered in header');
    // Hub peer roster section is attached to the card.
    const hubSection = card.children.find(c => c.classList && c.classList.contains('vpn-hub-section'));
    assert.ok(hubSection, 'hub peer roster section present');
});

test('T-VPNRENDER: vpn panel handles missing path field (fallback to direct bool)', async () => {
    const data = {
        links: [
            {
                id: 1,
                link_id: 'site-a',
                role: 'vpnconnector',
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

    const side = cardOf(el).children[1].children[0];
    // Should render "Direct" even though path is missing (falls back to direct bool).
    assert.ok(side.innerHTML.includes('Direct'), 'fallback to direct bool works');
});

test('T-VPNRENDER: vpn panel with empty links shows empty state', async () => {
    const data = { links: [] };

    const el = document.createElement('div');
    await vpnPanel.render(el, data);

    assert.ok(el.innerHTML.includes('No VPN links active'), 'empty state message shown');
});
