/**
 * T-CARRIER-DEDUP: the secret panel must show ONE consumer row per logical
 * consumer regardless of `--carriers`. The backend fix prevents extra relay
 * carriers from registering, but the UI also folds away port-less carrier rows
 * defensively (D4) so an OLDER server cannot produce the spurious "N/A" rows seen
 * in the field. Two distinct consumers from the same host are both preserved.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import secretPanel from '../../src/admin_ui/panels/secret.js';

const BASE = {
    secret_id: 'dufs',
    role: 'secretconsumer',
    carriers: 4,
    udp: false,
    auto_reconnect: false,
    basic_auth: false,
    webserver_log: false,
    active: 0,
    uptime_secs: 30,
    relay_tx_bytes: 0,
    relay_rx_bytes: 0,
    notes: null,
};

// The consumer section is rendered last in a card body (providers first, if any).
function consumerRowCount(el) {
    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const section = body.children[body.children.length - 1];
    const table = section.children[1];
    const tbody = table.children[1];
    return tbody.children.length;
}

test('T-CARRIER-DEDUP: 1 primary + 3 port-less carriers collapse to 1 row', async () => {
    const data = [
        { ...BASE, id: 1, peer: '82.54.81.19:38944', local_proxy_port: 5009, notes: 'T3' },
        { ...BASE, id: 2, peer: '82.54.81.19:38950', local_proxy_port: null },
        { ...BASE, id: 3, peer: '82.54.81.19:38954', local_proxy_port: null },
        { ...BASE, id: 4, peer: '82.54.81.19:38968', local_proxy_port: null },
    ];
    const el = document.createElement('div');
    await secretPanel.render(el, data);
    assert.equal(consumerRowCount(el), 1, 'three carrier rows fold into the one logical consumer');
});

test('T-CARRIER-DEDUP: two real consumers from the same host are both kept', async () => {
    const data = [
        { ...BASE, id: 1, peer: '82.54.81.19:40000', local_proxy_port: 5007 },
        { ...BASE, id: 2, peer: '82.54.81.19:40001', local_proxy_port: 5008 },
    ];
    const el = document.createElement('div');
    await secretPanel.render(el, data);
    assert.equal(consumerRowCount(el), 2, 'distinct port-bearing consumers are not merged');
});

test('T-CARRIER-DEDUP: a backend-fixed payload (no carrier rows) is unchanged', async () => {
    const data = [
        { ...BASE, id: 1, peer: '82.54.81.19:38944', local_proxy_port: 5009, notes: 'T3' },
    ];
    const el = document.createElement('div');
    await secretPanel.render(el, data);
    assert.equal(consumerRowCount(el), 1, 'single consumer renders one row');
});
