/**
 * T-VHOST-PARITY: the Vhost section must mirror the Tunnels section — same
 * columns/logic (Subdomain in place of Port) plus the two vhost-only trailing
 * columns (Direct Opens, Headers). Flags + execution info must render.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import vhostPanel from '../../src/admin_ui/panels/vhost.js';
import tunnelsPanel from '../../src/admin_ui/panels/tunnels.js';

const SAMPLE = [
    {
        subdomain: 'demo',
        peer: '203.0.113.7:443',
        notes: 'prod edge',
        basic_auth: true,
        udp: true,
        auto_reconnect: true,
        webserver_log: true,
        carriers: 4,
        tls: true,
        uptime_secs: 600,
        relay_tx_bytes: 1024,
        relay_rx_bytes: 2048,
        active: 3,
        direct_stream_opens: 7,
        request_headers: ['X-A'],
        response_headers: ['X-B'],
        request_header_pairs: [['X-A', '1']],
        response_header_pairs: [['X-B', '2']],
        direct_pool: 2
    }
];

function headerLabels(el) {
    const table = el.children[0];
    const headerRow = table.children[0].children[0];
    return Array.from(headerRow.children).map((th) => th.textContent);
}

test('T-VHOST-PARITY: vhost shares the Tunnels execution columns', async () => {
    const el = document.createElement('div');
    await vhostPanel.render(el, SAMPLE);
    const labels = headerLabels(el);

    // The 7 execution columns Tunnels uses (Port → Subdomain) must all be present.
    for (const col of ['Peer', 'Flags', 'Connections', 'Uptime', 'TX', 'RX', 'Notes']) {
        assert.ok(labels.includes(col), `Vhost table must have "${col}" column`);
    }
    // Identity column is Subdomain (the Port analog), not Port.
    assert.ok(labels.includes('Subdomain'), 'Vhost identity column is Subdomain');
    assert.ok(!labels.includes('Port'), 'Vhost must not show a Port column');
    // Vhost-only trailing columns retained.
    assert.ok(labels.includes('Direct Opens'), 'vhost-only Direct Opens retained');
    assert.ok(labels.includes('Headers'), 'vhost-only Headers retained');
});

test('T-VHOST-PARITY: vhost shares every Tunnels execution column (no drift)', async () => {
    const tEl = document.createElement('div');
    await tunnelsPanel.render(tEl, [{ public_port: 80, peer: 'x', active: 0, carriers: 1 }]);
    const tunnelCols = new Set(headerLabels(tEl));
    tunnelCols.delete('Port'); // Subdomain is the vhost analog

    const vEl = document.createElement('div');
    await vhostPanel.render(vEl, SAMPLE);
    const vhostCols = new Set(headerLabels(vEl));

    for (const col of tunnelCols) {
        assert.ok(vhostCols.has(col), `Vhost missing shared Tunnels column "${col}"`);
    }
});

// Recursively accumulate text across nested stub nodes (badges live at
// td → span → badge, so a shallow textContent read misses them).
function deepText(node) {
    let t = node.textContent || '';
    if (node.children) for (const c of node.children) t += ' ' + deepText(c);
    return t;
}

test('T-VHOST-PARITY: vhost renders flags, uptime and bytes from the new fields', async () => {
    const el = document.createElement('div');
    await vhostPanel.render(el, SAMPLE);
    const tbody = el.children[0].children[1];
    const row = tbody.children[0];
    const text = Array.from(row.children).map(deepText).join(' | ');

    // Uptime formatted (10m), TX/RX humanized (KB).
    assert.ok(/10m/.test(text), 'uptime rendered');
    assert.ok(/KB/.test(text), 'tx/rx humanized');
    // Flags cell carries the shared badges (UDP, Basic Auth, carriers, weblog, TLS).
    assert.ok(text.includes('UDP'), 'UDP badge');
    assert.ok(text.includes('Basic Auth'), 'Basic Auth badge');
    assert.ok(text.includes('TLS'), 'TLS badge');
    assert.ok(text.includes('Weblog'), 'Weblog badge');
    assert.ok(/x4/.test(text), 'carriers badge');
    assert.ok(text.includes('Auto-reconnect'), 'Auto-reconnect badge');
});
