/**
 * T-VHOSTPATH: the Vhost table's `Path` column (phase 05.3).
 *
 * The counters alone could not answer "is this tunnel using --udp right now?":
 * during a measured total UDP blackout the opens counter climbed from 1 to 12
 * while the fallback counter stayed at 0, so a tunnel that had fallen back for
 * every connection looked identical to a healthy direct one. This column is
 * that answer, so its three states — and specifically the `--udp`-tunnel-on-
 * relay state — are worth pinning.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import vhostPanel from '../../src/admin_ui/panels/vhost.js';

function headers(el) {
    return Array.from(el.children[0].children[0].children[0].children).map(th => th.textContent);
}

// Badges live at td → span, so a shallow textContent read misses them (same
// helper shape as the parity test).
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
    await vhostPanel.render(el, [{ id: 1, subdomain: 'api', active: 0, carriers: 1, ...entry }]);
    return el;
}

test('T-VHOSTPATH: a tunnel on the QUIC direct path reports direct', async () => {
    const el = await renderOne({ udp: true, current_path: 'direct', direct_stream_opens: 4, direct_fallbacks: 0 });
    assert.equal(pathCellText(el), 'direct');
});

test('T-VHOSTPATH: a --udp tunnel serving on the relay reports its fallback count', async () => {
    const el = await renderOne({ udp: true, current_path: 'relay', direct_stream_opens: 4, direct_fallbacks: 7 });
    assert.equal(
        pathCellText(el),
        'relay (7 fallbacks)',
        'the interesting state is "asked for --udp, currently on the relay"'
    );
});

test('T-VHOSTPATH: a single fallback is not pluralized', async () => {
    const el = await renderOne({ udp: true, current_path: 'relay', direct_fallbacks: 1 });
    assert.equal(pathCellText(el), 'relay (1 fallback)');
});

test('T-VHOSTPATH: a relay-only tunnel is plain relay, not a fallback', async () => {
    const el = await renderOne({ udp: false, current_path: 'relay', direct_fallbacks: 0 });
    assert.equal(pathCellText(el), 'relay');
});

test('T-VHOSTPATH: an old server without the field renders unknown, never crashes', async () => {
    const el = await renderOne({ udp: true, direct_stream_opens: 2 });
    assert.equal(pathCellText(el), 'unknown');
});
