import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import registry from '../../src/admin_ui/registry.js';
import panel from '../../src/admin_ui/panels/ssh-jump.js';

const SAMPLE = [{
    hostname: '<script>alert(1)</script>.ssh.example.test',
    ssh_port: 22,
    peer: '203.0.113.8:44000',
    provider_type: 'native',
    notes: '<img src=x onerror=alert(1)>',
    requested_carriers: 4,
    effective_carriers: 2,
    udp_requested: true,
    udp_active: false,
    direct_carriers: 0,
    direct_stream_opens: 7,
    direct_fallbacks: 2,
    active_connections: 3,
    uptime_secs: 65,
    relay_tx_bytes: 1024,
    relay_rx_bytes: 2048,
    direct_tx_bytes: 0,
    direct_rx_bytes: 0,
}];

test('T-JH-UI: registry exposes dedicated Jump Hosts panel', () => {
    const registered = registry.find((entry) => entry.id === 'ssh-jump');
    assert.equal(registered?.endpoint, '/admin/api/v1/ssh-jump');
});

test('T-JH-UI: panel renders operational fields and escapes untrusted text', async () => {
    const el = document.createElement('div');
    await panel.render(el, SAMPLE);
    const table = el.children[0];
    const headers = Array.from(table.children[0].children[0].children)
        .map((cell) => cell.textContent);
    for (const name of ['Hostname', 'Provider', 'Path', 'Carriers', 'Connections', 'TX', 'RX']) {
        assert.ok(headers.includes(name), `missing ${name}`);
    }
    const row = table.children[1].children[0];
    const hostname = row.children[0].textContent;
    const notes = row.children[row.children.length - 1].children[0];
    assert.ok(!hostname.includes('<script>'), 'hostname escaped');
    assert.equal(notes.innerHTML, '', 'notes never parsed as HTML');
    assert.equal(notes.textContent, SAMPLE[0].notes, 'notes rendered through textContent');
    assert.equal(row.children[4].children[0].children[0].textContent, 'Relay fallback');
});
