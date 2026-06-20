/**
 * T-FLAG-COVERAGE: every operator flag that reaches the server must be visible
 * somewhere — as a Flags badge (via the shared `flagBadges`) or, for fields with
 * no dedicated badge/column, in the detail modal (which renders every non-`_`
 * field via `detailRows`). This locks the "show ALL flags" requirement.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { flagBadges } from '../../src/admin_ui/ui.js';
import { detailRows } from '../../src/admin_ui/modal.js';

test('flagBadges covers every flag for a fully-flagged entry', () => {
    const labels = flagBadges({
        https: true,
        force_https: true,
        tls: true,
        basic_auth: true,
        udp: true,
        carriers: 8,
        auto_reconnect: true,
        webserver_log: true,
        upnp: true,
        try_port_prediction: true,
        nat_udp_preferred_port: 443
    }).map((b) => b.label);

    assert.ok(labels.includes('HTTPS'));
    assert.ok(labels.includes('Force-HTTPS'));
    assert.ok(labels.includes('TLS'));
    assert.ok(labels.includes('Basic Auth'));
    assert.ok(labels.includes('UDP'));
    assert.ok(labels.some((l) => l.includes('8')), 'carriers count');
    assert.ok(labels.includes('Auto-reconnect'));
    assert.ok(labels.includes('Weblog'));
    assert.ok(labels.includes('UPnP'));
    assert.ok(labels.includes('Port-Pred'));
    assert.ok(labels.some((l) => l === 'NAT:443'));
});

test('flagBadges shows nothing for a bare entry', () => {
    assert.equal(flagBadges({ carriers: 1 }).length, 0);
    assert.equal(flagBadges({}).length, 0);
});

test('flagBadges uses only CSS-defined badge kinds', () => {
    const valid = new Set(['primary', 'success', 'warning', 'danger', 'default']);
    const all = flagBadges({
        https: true, force_https: true, tls: true, basic_auth: true,
        udp: true, carriers: 2, auto_reconnect: true, webserver_log: true
    });
    for (const b of all) assert.ok(valid.has(b.kind), `kind ${b.kind} must exist in style.css`);
});

test('flagBadges treats both https (public) and tls (vhost) as a TLS-class badge', () => {
    assert.ok(flagBadges({ https: true }).some((b) => b.kind === 'primary' && b.label === 'HTTPS'));
    assert.ok(flagBadges({ tls: true }).some((b) => b.kind === 'primary' && b.label === 'TLS'));
});

test('detail modal exposes any view field with no dedicated column (catch-all)', () => {
    // A vhost view carries fields not in the table (direct_pool, header pairs);
    // detailRows must surface every non-underscore field as a row.
    const rows = detailRows({
        subdomain: 'demo',
        webserver_log: true,
        direct_pool: 3,
        request_header_pairs: [['X-A', '1']],
        _entry: { hidden: true }
    });
    const labels = rows.map((r) => r.label);
    assert.ok(labels.includes('Webserver Log'), 'webserver_log shown in modal');
    assert.ok(labels.includes('Direct Pool'), 'direct_pool shown in modal');
    assert.ok(labels.some((l) => l.includes('Request Header Pairs')), 'header pairs shown');
    assert.ok(!labels.some((l) => l.toLowerCase().includes('entry')), 'underscore fields skipped');
});
