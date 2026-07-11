/**
 * Metrics panel: server-side Rate TX/RX (`rate_tx_bps`/`rate_rx_bps`) plus the
 * live counters (active_connections, auth_failures, conn_rejections,
 * direct_fallbacks), the SSH-tunnel count and the transport breakdown.
 *
 * The old client-side `rateFromSamples` delta was removed — the server now runs
 * its own 1 s sampler — so these tests assert the panel renders the SERVER
 * values, and specifically that a 0 rate renders as "0 …/s" and NEVER the "—"
 * placeholder (the originally-reported "Rate always 0/—" bug).
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import metricsPanel from '../../src/admin_ui/panels/metrics.js';
import { fmtBytes } from '../../src/admin_ui/ui.js';

// The dom-stub does not parse innerHTML into a live tree, so gather every
// `_html`/`_text` string across the rendered subtree (same approach as
// vhost-parity.test.js's collectText helper).
function allText(node) {
    let s = `${node._html || ''} ${node._text || ''}`;
    for (const c of node.children || []) s += ` ${allText(c)}`;
    return s;
}

const base = {
    uptime_secs: 3600,
    mem_rss_bytes: 104857600,
    bandwidth_tx_bytes: 1000000,
    bandwidth_rx_bytes: 500000,
    public_tunnels: 2,
    secret_tunnels: 1,
    vhost_domains: 1,
};

test('metrics panel renders server-side Rate TX/RX values', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, rate_tx_bps: 2048, rate_rx_bps: 1024 });
    const txt = allText(el);
    assert.ok(txt.includes('Rate TX'), 'has Rate TX label');
    assert.ok(txt.includes('Rate RX'), 'has Rate RX label');
    assert.ok(txt.includes(`${fmtBytes(2048)}/s`), 'shows server-provided tx rate');
    assert.ok(txt.includes(`${fmtBytes(1024)}/s`), 'shows server-provided rx rate');
});

test('metrics panel shows a 0 rate as "0 …/s", never the "—" placeholder', async () => {
    // This is the exact reported bug: Rate TX/RX must be a real value, not "—".
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, rate_tx_bps: 0, rate_rx_bps: 0 });
    const txt = allText(el);
    assert.ok(txt.includes(`${fmtBytes(0)}/s`), 'zero rate renders as a bytes/s value');
    assert.ok(!txt.includes('—'), 'no em-dash placeholder anywhere in the panel');
});

test('metrics panel renders the live counters when present', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, {
        ...base,
        rate_tx_bps: 0,
        rate_rx_bps: 0,
        active_connections: 42,
        auth_failures: 7,
        conn_rejections: 3,
        direct_fallbacks: 5,
    });
    const txt = allText(el);
    for (const [label, val] of [
        ['Active Connections', '42'],
        ['Auth Failures', '7'],
        ['Conn Rejections', '3'],
        ['Direct Fallbacks', '5'],
    ]) {
        assert.ok(txt.includes(label), `has "${label}" label`);
        assert.ok(txt.includes(val), `"${label}" shows value ${val}`);
    }
});

test('metrics panel renders SSH tunnel count + transport breakdown', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, {
        ...base,
        rate_tx_bps: 0,
        rate_rx_bps: 0,
        ssh_tunnels: 4,
        transport_bore: 2,
        transport_ssh: 3,
    });
    const txt = allText(el);
    assert.ok(txt.includes('SSH Tunnels'), 'has SSH Tunnels count');
    assert.ok(txt.includes('Transport Breakdown'), 'has transport breakdown section');
    assert.ok(txt.includes('Bore'), 'has Bore row');
    assert.ok(txt.includes('2'), 'bore count rendered');
    assert.ok(txt.includes('3'), 'ssh count rendered');
});

test('metrics panel omits optional counters/transport when absent', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, rate_tx_bps: 0, rate_rx_bps: 0 });
    const txt = allText(el);
    assert.ok(el.children.length > 0, 'panel still renders the core cards');
    assert.ok(!txt.includes('Auth Failures'), 'no Auth Failures row when field absent');
    assert.ok(!txt.includes('Transport Breakdown'), 'no transport breakdown when fields absent');
});
