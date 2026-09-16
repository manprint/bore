/**
 * Metrics panel, Web Transfer section (plan 001, sub-phase 6.2).
 *
 * Two rules, both of which have bitten this project before:
 *   - the section appears ONLY when the service is enabled, because every
 *     field is `null` on a server without it and a row of zeros would claim a
 *     browser surface that does not exist;
 *   - inside it, a live gauge that reads 0 must RENDER 0. P-11's frontend half:
 *     "no free relay slot" is the alarming value, and a truthiness guard hides
 *     exactly the value the operator needs.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import metricsPanel from '../../src/admin_ui/panels/metrics.js';
import { fmtBytes } from '../../src/admin_ui/ui.js';

function allText(node) {
    let s = `${node._html || ''} ${node._text || ''}`;
    for (const c of node.children || []) s += ` ${allText(c)}`;
    return s;
}

const base = {
    uptime_secs: 60,
    mem_rss_bytes: 1048576,
    bandwidth_tx_bytes: 0,
    bandwidth_rx_bytes: 0,
    rate_tx_bps: 0,
    rate_rx_bps: 0,
};

const web = {
    web_transfer_rooms_current: 2,
    web_transfer_peers_current: 5,
    web_transfer_offers_current: 7,
    web_transfer_metadata_bytes_current: 4096,
    web_transfer_transfers_active: 1,
    web_transfer_relays_active: 3,
    web_transfer_relay_slots_available: 0,
    web_transfer_relay_ciphertext_bytes_total: 1048576,
    web_transfer_direct_commits_total: 9,
    web_transfer_relay_commits_total: 4,
    web_transfer_completed_total: 11,
    web_transfer_cancelled_total: 2,
    web_transfer_rejected_total: 6,
};

test('web transfer metrics are hidden when the service is disabled', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base });
    assert.ok(!allText(el).includes('Web Transfer'), 'no section without the service');
});

test('web transfer metrics render every gauge and total when enabled', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, ...web });
    const txt = allText(el);
    for (const label of [
        'Web Transfer',
        'Rooms',
        'Peers',
        'Offers',
        'Catalog Metadata',
        'Transfers Active',
        'Relays Active',
        'Relay Slots Free',
        'Relay Ciphertext',
        'Direct Commits',
        'Relay Commits',
        'Completed',
        'Cancelled',
        'Rejected',
    ]) {
        assert.ok(txt.includes(label), `missing row: ${label}`);
    }
    assert.ok(txt.includes(fmtBytes(4096)), 'metadata bytes formatted');
    assert.ok(txt.includes(fmtBytes(1048576)), 'relay ciphertext formatted');
});

test('a zero relay slot count is shown, never hidden', async () => {
    // Red-check: rendering the row behind `if (value)` makes this fail, which
    // is the whole point — 0 free slots is the state worth an alarm.
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, ...web });
    const txt = allText(el);
    const at = txt.indexOf('Relay Slots Free');
    assert.ok(at >= 0, 'row present');
    assert.ok(/Relay Slots Free[\s\S]{0,200}?>0</.test(txt.slice(at)), 'the zero is rendered');
});

test('a room count of zero still shows the section', async () => {
    // Zero rooms on an ENABLED server is a fact, not an absence.
    const el = document.createElement('div');
    await metricsPanel.render(el, {
        ...base,
        ...web,
        web_transfer_rooms_current: 0,
        web_transfer_peers_current: 0,
    });
    assert.ok(allText(el).includes('Web Transfer'), 'section present at zero');
});
