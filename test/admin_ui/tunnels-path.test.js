/**
 * T-PUBPATH: the Public (Tunnels) table's `Path` column.
 *
 * The public-tunnel twin of T-VHOSTPATH, and it exists because the two
 * sections were NOT symmetric: a public `bore local --udp` tunnel published no
 * per-tunnel direct-path state at all, so a tunnel that had fallen back to the
 * warm relay for every single connection rendered exactly like a healthy
 * direct one. The server-wide `direct_fallbacks` metric cannot close that gap —
 * it cannot say WHICH tunnel is degraded.
 *
 * The three states pinned here are the same three the Vhost panel pins, and
 * deliberately so (D5: one flag/badge behaviour across sections).
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import tunnelsPanel from '../../src/admin_ui/panels/tunnels.js';

function headers(el) {
    return Array.from(el.children[0].children[0].children[0].children).map(th => th.textContent);
}

// Badges live at td → span, so a shallow textContent read misses them.
function deepText(node) {
    let t = node.textContent || '';
    if (node.children) for (const c of node.children) t += deepText(c);
    return t;
}

function pathCellText(el) {
    const cols = headers(el);
    const idx = cols.indexOf('Path');
    assert.ok(idx >= 0, 'the table has a Path column');
    const bodyRow = el.children[0].children[1].children[0];
    return deepText(bodyRow.children[idx]).trim();
}

async function renderOne(entry) {
    const el = document.createElement('div');
    await tunnelsPanel.render(el, [{ id: 1, public_port: 9000, peer: '203.0.113.7:5000', active: 0, carriers: 1, ...entry }]);
    return el;
}

test('T-PUBPATH: a public tunnel on the QUIC direct path reports direct', async () => {
    const el = await renderOne({ udp: true, current_path: 'direct', direct_stream_opens: 4, direct_fallbacks: 0 });
    assert.equal(pathCellText(el), 'direct');
});

test('T-PUBPATH: a --udp public tunnel serving on the relay reports its fallback count', async () => {
    const el = await renderOne({ udp: true, current_path: 'relay', direct_stream_opens: 4, direct_fallbacks: 7 });
    assert.equal(
        pathCellText(el),
        'relay (7 fallbacks)',
        'the interesting state is "asked for --udp, currently on the relay"'
    );
});

test('T-PUBPATH: a single fallback is not pluralized', async () => {
    const el = await renderOne({ udp: true, current_path: 'relay', direct_fallbacks: 1 });
    assert.equal(pathCellText(el), 'relay (1 fallback)');
});

test('T-PUBPATH: a relay-only public tunnel is plain relay, not a fallback', async () => {
    const el = await renderOne({ udp: false, current_path: 'relay', direct_fallbacks: 0 });
    assert.equal(pathCellText(el), 'relay');
});

test('T-PUBPATH: an old server without the field renders unknown, never crashes', async () => {
    const el = await renderOne({ udp: true, direct_stream_opens: 2 });
    assert.equal(pathCellText(el), 'unknown');
});
