/**
 * Metrics panel, Fast Link section (plan 004, sub-phase 1.3).
 *
 * Same two rules as the Web Transfer section this mirrors:
 *   - the section appears ONLY when the service is enabled (`data.fast_link`
 *     is a non-null object; D6: `null` means the service is off);
 *   - inside it, a live gauge that reads 0 must RENDER 0 — a truthiness
 *     guard would hide exactly the value an operator needs (P-11).
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

const fastLink = {
    waiting: 2,
    streaming: 1,
    uploads_total: 9,
    completed_total: 5,
    failed_total: 1,
    expired_total: 2,
    rearmed_total: 3,
    previews_blocked_total: 4,
    auth_failures_total: 0,
    rejected_busy_total: 0,
    bytes_total: 1048576,
};

test('fast link metrics are hidden when the service is disabled (null)', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, fast_link: null });
    assert.ok(!allText(el).includes('Fast Link'), 'no section without the service');
});

test('fast link metrics are hidden when the field is absent entirely', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base });
    assert.ok(!allText(el).includes('Fast Link'), 'no section without the field');
});

test('fast link metrics render every gauge and total when enabled', async () => {
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, fast_link: fastLink });
    const txt = allText(el);
    for (const label of [
        'Fast Link',
        'Waiting',
        'Streaming',
        'Uploads',
        'Completed',
        'Failed',
        'Expired',
        'Re-armed',
        'Previews Blocked',
        'Auth Failures',
        'Rejected (Busy)',
        'Bytes',
    ]) {
        assert.ok(txt.includes(label), `missing row: ${label}`);
    }
    assert.ok(txt.includes(fmtBytes(1048576)), 'bytes formatted');
});

test('zero-valued counters are shown, never hidden', async () => {
    // Red-check: rendering a row behind `if (value)` makes this fail, which
    // is the whole point — 0 auth failures / 0 busy rejections is the state
    // worth confirming, not a value that should silently disappear.
    const el = document.createElement('div');
    await metricsPanel.render(el, { ...base, fast_link: fastLink });
    const txt = allText(el);
    for (const label of ['Auth Failures', 'Rejected (Busy)']) {
        const at = txt.indexOf(label);
        assert.ok(at >= 0, `${label} row present`);
        const escaped = label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
        assert.ok(
            new RegExp(`${escaped}[\\s\\S]{0,200}?>0<`).test(txt.slice(at)),
            `the zero is rendered for ${label}`
        );
    }
});

test('a waiting count of zero still shows the section', async () => {
    // Zero waiting/streaming on an ENABLED server is a fact, not an absence.
    const el = document.createElement('div');
    await metricsPanel.render(el, {
        ...base,
        fast_link: { ...fastLink, waiting: 0, streaming: 0 },
    });
    assert.ok(allText(el).includes('Fast Link'), 'section present at zero');
});
