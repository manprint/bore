/**
 * BUG-3: every operator-visible flag must surface as a badge. Reference-scenario
 * client: --udp --carriers 4 --https --force-https --auto-reconnect.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { tunnelBadges } from '../../src/admin_ui/panels/tunnels.js';

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
