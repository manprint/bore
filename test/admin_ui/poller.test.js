/**
 * BUG-0: prove the poll timer actually invokes the refresh function (the old
 * code dispatched an event nobody listened to).
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import { createPoller, DEFAULT_REFRESH_MS } from '../../src/admin_ui/poller.js';

test('T-FE-POLL0: default timers path is browser-safe and restartable', () => {
    const originalSetInterval = globalThis.setInterval;
    const originalClearInterval = globalThis.clearInterval;
    const capture = {
        setCalls: 0,
        clearCalls: 0,
        lastMs: null,
        lastFn: null,
        lastHandle: null,
        nextHandle: 0,
        ticks: 0,
    };

    try {
        globalThis.setInterval = function(fn, ms) {
            if (this !== globalThis) {
                throw new TypeError('Illegal invocation');
            }
            capture.setCalls += 1;
            capture.lastFn = fn;
            capture.lastMs = ms;
            return ++capture.nextHandle;
        };
        globalThis.clearInterval = function(handle) {
            if (this !== globalThis) {
                throw new TypeError('Illegal invocation');
            }
            capture.clearCalls += 1;
            capture.lastHandle = handle;
        };

        const p = createPoller(() => capture.ticks++);

        p.start(3000);
        assert.equal(capture.lastMs, 3000, 'arms with the panel refreshMs');
        assert.equal(capture.setCalls, 1, 'schedules exactly one timer');
        assert.equal(typeof capture.lastFn, 'function', 'captures interval callback');
        assert.ok(p.isRunning());

        capture.lastFn();
        capture.lastFn();
        assert.equal(capture.ticks, 2, 'each tick calls refreshFn (the BUG-0 fix)');

        p.start(5000);
        assert.equal(capture.clearCalls, 1, 'restart clears the previous timer');
        assert.equal(capture.lastHandle, 1, 'restart clears prior handle');
        assert.equal(capture.lastMs, 5000, 'restart re-arms at new interval');
        assert.equal(capture.setCalls, 2, 'restart schedules a fresh timer');

        p.stop();
        assert.equal(capture.clearCalls, 2, 'stop clears current timer');
        assert.equal(capture.lastHandle, 2, 'stop clears latest handle');
        assert.ok(!p.isRunning());
    } finally {
        globalThis.setInterval = originalSetInterval;
        globalThis.clearInterval = originalClearInterval;
    }
});

test('poller injected timers still invoke refreshFn on each interval tick and are restartable', () => {
    let calls = 0;
    const captured = {};
    const timers = {
        setInterval: (fn, ms) => {
            captured.fn = fn;
            captured.ms = ms;
            return 42;
        },
        clearInterval: (h) => {
            captured.cleared = h;
        },
    };
    const p = createPoller(() => calls++, timers);

    p.start(3000);
    assert.equal(captured.ms, 3000, 'arms with the panel refreshMs');
    assert.ok(p.isRunning());

    captured.fn();
    captured.fn();
    assert.equal(calls, 2, 'each tick calls refreshFn (the BUG-0 fix)');

    p.start(5000);
    assert.equal(captured.cleared, 42, 'restart clears the previous timer');

    p.stop();
    assert.ok(!p.isRunning());
});

test('poller does not arm when refreshMs <= 0', () => {
    let started = false;
    const timers = {
        setInterval: () => {
            started = true;
            return 1;
        },
        clearInterval: () => {},
    };
    const p = createPoller(() => {}, timers);
    p.start(0);
    assert.ok(!started, 'refreshMs<=0 must not schedule');
    assert.ok(!p.isRunning());
});

test('DEFAULT_REFRESH_MS is 30000', () => {
    assert.equal(DEFAULT_REFRESH_MS, 30000, 'must be 30 seconds for consistent polling');
});
