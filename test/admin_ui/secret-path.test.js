/**
 * T-SECPATH: the Secret panel's `Path` column (S-1).
 *
 * The secret registry was the only one of the four with no direct-path
 * observability, and the reason is structural: for a vhost, public or ssh-jump
 * tunnel the SERVER is an endpoint of the QUIC direct connection and can simply
 * look, while a secret tunnel's direct path runs consumer↔provider and never
 * reaches the server at all. The consumer therefore reports it, and this column
 * is where that report surfaces.
 *
 * What is worth pinning is the set of states, because each one means something
 * different to the operator reading the row:
 *
 *   direct                  the tunnel is on the QUIC path right now
 *   relay (N fallbacks)     it asked for --udp and is NOT — the actionable one
 *   relay                   it never asked for --udp; one path, no mystery
 *   unknown                 the server genuinely cannot answer (no report yet,
 *                           or a provider row, which never reports)
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import secretPanel from '../../src/admin_ui/panels/secret.js';

const BASE = {
    secret_id: 'dufs',
    carriers: 1,
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

// Badges live at td → span, so a shallow read misses them.
function deepText(node) {
    let t = node.textContent || '';
    if (node.children) for (const c of node.children) t += deepText(c);
    return t;
}

// Walk to the LAST role section of the first card: providers render first, so
// a consumer-only payload and a provider+consumer payload both land here.
function lastSection(el) {
    const body = el.children[0].children[0].children[1];
    return body.children[body.children.length - 1];
}

function headers(el) {
    const table = lastSection(el).children[1];
    return Array.from(table.children[0].children[0].children).map(th => th.textContent);
}

function pathCell(el) {
    const idx = headers(el).indexOf('Path');
    assert.ok(idx >= 0, 'the table has a Path column');
    const table = lastSection(el).children[1];
    return table.children[1].children[0].children[idx];
}

async function renderOne(entry) {
    const el = document.createElement('div');
    await secretPanel.render(el, [{
        ...BASE,
        id: 1,
        role: 'secretconsumer',
        peer: '10.0.0.2:40000',
        local_proxy_port: 5009,
        ...entry,
    }]);
    return el;
}

test('T-SECPATH: a consumer on the QUIC direct path reports direct', async () => {
    const el = await renderOne({ udp: true, current_path: 'direct', direct_fallbacks: 0 });
    assert.equal(deepText(pathCell(el)).trim(), 'direct');
});

test('T-SECPATH: a --udp consumer serving on the relay reports its fallback count', async () => {
    const el = await renderOne({ udp: true, current_path: 'relay', direct_fallbacks: 3 });
    assert.equal(
        deepText(pathCell(el)).trim(),
        'relay (3 fallbacks)',
        'the interesting state is "asked for --udp, currently on the relay"'
    );
});

test('T-SECPATH: a single fallback is not pluralized', async () => {
    const el = await renderOne({ udp: true, current_path: 'relay', direct_fallbacks: 1 });
    assert.equal(deepText(pathCell(el)).trim(), 'relay (1 fallback)');
});

test('T-SECPATH: a relay-only consumer is plain relay, not a fallback', async () => {
    const el = await renderOne({ udp: false, current_path: 'relay', direct_fallbacks: 0 });
    assert.equal(deepText(pathCell(el)).trim(), 'relay');
});

test('T-SECPATH: the fallback reason rides as the cell tooltip, not as text', async () => {
    const el = await renderOne({
        udp: true,
        current_path: 'relay',
        direct_fallbacks: 1,
        path_reason: 'no udp-capable provider registered',
    });
    const cell = pathCell(el);
    assert.equal(cell.title, 'no udp-capable provider registered');
    assert.ok(
        !deepText(cell).includes('provider'),
        'a free sentence must not widen the column'
    );
});

test('T-SECPATH: an old server without the field renders unknown, never crashes', async () => {
    const el = await renderOne({ udp: true });
    assert.equal(deepText(pathCell(el)).trim(), 'unknown');
});

test('T-SECPATH: a --udp provider row is unknown — it is not the side that reports', async () => {
    const el = document.createElement('div');
    await secretPanel.render(el, [{
        ...BASE,
        id: 1,
        role: 'secretprovider',
        peer: '10.0.0.3:40000',
        local_host: '127.0.0.1',
        local_port: 8080,
        udp: true,
        current_path: 'unknown',
    }]);
    assert.equal(deepText(pathCell(el)).trim(), 'unknown');
});
