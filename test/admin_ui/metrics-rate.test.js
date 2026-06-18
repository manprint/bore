/**
 * BUG-5: the bandwidth fields are cumulative totals; the rate must be derived
 * from two samples (delta bytes / delta time), guarding against NaN/Infinity.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { rateFromSamples } from '../../src/admin_ui/panels/metrics.js';

test('rateFromSamples computes bytes/second from a delta', () => {
    const r = rateFromSamples({ tx: 0, rx: 0, t: 0 }, { tx: 1_000_000, rx: 500_000, t: 2 });
    assert.equal(r.txbps, 500_000);
    assert.equal(r.rxbps, 250_000);
});

test('rateFromSamples returns null on first sample', () => {
    assert.equal(rateFromSamples(null, { tx: 10, rx: 10, t: 1 }), null);
});

test('rateFromSamples guards dt<=0 (no NaN/Infinity)', () => {
    assert.equal(rateFromSamples({ tx: 0, rx: 0, t: 5 }, { tx: 9, rx: 9, t: 5 }), null);
    assert.equal(rateFromSamples({ tx: 0, rx: 0, t: 6 }, { tx: 9, rx: 9, t: 5 }), null);
});

test('rateFromSamples clamps a counter reset to 0 (never negative)', () => {
    const r = rateFromSamples({ tx: 100, rx: 100, t: 0 }, { tx: 0, rx: 0, t: 1 });
    assert.equal(r.txbps, 0);
    assert.equal(r.rxbps, 0);
});
