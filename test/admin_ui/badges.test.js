/**
 * BUG-3: every operator-visible flag must surface as a badge. Reference-scenario
 * client: --udp --carriers 4 --https --force-https --auto-reconnect.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { tunnelBadges } from '../../src/admin_ui/panels/tunnels.js';
import { flagBadges } from '../../src/admin_ui/ui.js';

test('tunnelBadges surfaces all reference-scenario flags', () => {
    const labels = tunnelBadges({
        https: true,
        force_https: true,
        udp: true,
        carriers: 4,
        auto_reconnect: true,
        basic_auth: false,
    }).map((b) => b.label);

    assert.ok(labels.includes('HTTPS'));
    assert.ok(labels.includes('Force-HTTPS'));
    assert.ok(labels.includes('UDP'));
    assert.ok(labels.includes('Auto-reconnect'));
    assert.ok(labels.some((l) => l.includes('4')), 'carriers count shown');
    assert.ok(!labels.includes('Basic Auth'), 'basic_auth=false ⇒ no badge');
});

test('tunnelBadges hides carriers when single-connection (<=1)', () => {
    assert.ok(!tunnelBadges({ carriers: 1 }).some((b) => b.label.includes('carriers')));
    assert.ok(!tunnelBadges({ carriers: 0 }).some((b) => b.label.includes('carriers')));
});

test('tunnelBadges uses only CSS-defined badge kinds', () => {
    const valid = new Set(['primary', 'success', 'warning', 'danger', 'default']);
    const badges = tunnelBadges({
        https: true,
        force_https: true,
        basic_auth: true,
        udp: true,
        carriers: 2,
        auto_reconnect: true,
    });
    for (const b of badges) assert.ok(valid.has(b.kind), `kind ${b.kind} must exist in style.css`);
});

test('flagBadges renders upnp badge when true', () => {
    const labels = flagBadges({ upnp: true }).map((b) => b.label);
    assert.ok(labels.includes('UPnP'));
});

test('flagBadges renders try_port_prediction badge when true', () => {
    const labels = flagBadges({ try_port_prediction: true }).map((b) => b.label);
    assert.ok(labels.includes('Port-Pred'));
});

test('flagBadges renders nat_udp_preferred_port badge when > 0', () => {
    const labels = flagBadges({ nat_udp_preferred_port: 443 }).map((b) => b.label);
    assert.ok(labels.some((l) => l === 'NAT:443'));
});

test('flagBadges hides new badges when flags are false or zero', () => {
    const labels = flagBadges({
        upnp: false,
        try_port_prediction: false,
        nat_udp_preferred_port: 0,
        carriers: 1
    }).map((b) => b.label);
    assert.ok(!labels.some((l) => l.includes('UPnP')));
    assert.ok(!labels.some((l) => l.includes('Port-Pred')));
    assert.ok(!labels.some((l) => l.includes('NAT:')));
});

test('flagBadges regression: existing flags still work', () => {
    const labels = flagBadges({
        https: true,
        force_https: true,
        tls: true,
        basic_auth: true,
        udp: true,
        carriers: 8,
        auto_reconnect: true,
        webserver_log: true
    }).map((b) => b.label);

    assert.ok(labels.includes('HTTPS'));
    assert.ok(labels.includes('Force-HTTPS'));
    assert.ok(labels.includes('TLS'));
    assert.ok(labels.includes('Basic Auth'));
    assert.ok(labels.includes('UDP'));
    assert.ok(labels.some((l) => l.includes('8')), 'carriers count');
    assert.ok(labels.includes('Auto-reconnect'));
    assert.ok(labels.includes('Weblog'));
});
