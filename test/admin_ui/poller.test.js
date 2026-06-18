/**
 * BUG-0: prove the poll timer actually invokes the refresh function (the old
 * code dispatched an event nobody listened to).
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import { createPoller } from '../../src/admin_ui/poller.js';

test('poller invokes refreshFn on each interval tick and is restartable', () => {
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
