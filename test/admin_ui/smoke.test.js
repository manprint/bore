/**
 * Phase 0 smoke test: proves the zero-dep node:test + DOM-stub harness can
 * import the ES-module admin_ui sources and exercise a pure helper.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { fmtBytes, fmtDuration, badge } from '../../src/admin_ui/ui.js';

test('fmtBytes formats bytes human-readable', () => {
    assert.equal(fmtBytes(0), '0.00 B');
    assert.equal(fmtBytes(1024), '1.00 KB');
    assert.equal(fmtBytes(1536), '1.50 KB');
    assert.equal(fmtBytes(undefined), 'N/A');
    assert.equal(fmtBytes(-1), 'N/A');
});

test('fmtDuration formats seconds', () => {
    assert.equal(fmtDuration(0), '0s');
    assert.equal(fmtDuration(65), '1m 5s');
    assert.equal(fmtDuration(null), 'N/A');
});

test('badge builds an element via the DOM stub', () => {
    const b = badge('HTTPS', 'primary');
    assert.equal(b.tagName, 'SPAN');
    assert.ok(b.classList.contains('badge'));
    assert.ok(b.classList.contains('badge-primary'));
    assert.equal(b.textContent, 'HTTPS');
});
