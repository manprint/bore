/**
 * T-FE-POLL1 — auto-refresh WIRING regression test.
 *
 * poller.test.js proves the timer calls its callback in isolation. It does NOT
 * exercise the wiring in app.js (setupPolling reading `activePanel.refreshMs`,
 * the load-time arm, the hashchange re-arm) nor the router's refresh hook. That
 * untested seam is exactly where auto-refresh silently dies (the field bug: the
 * page froze and only a manual reload updated it).
 *
 * This test stands up just enough document/window/timer/fetch surface to import
 * the REAL app.js + router.js + registry and asserts that:
 *   1. on load with #/secret the poller arms with the panel's 30s interval,
 *   2. firing that interval RE-FETCHES the active endpoint (live refresh),
 *   3. a hashchange re-arms the poller and re-fetches the new endpoint.
 *
 * If any of those regress, auto-refresh is broken and this test fails.
 */
import test from 'node:test';
import assert from 'node:assert/strict';

// ---- Minimal DOM/window/timer/fetch harness (installed before importing app) ----

class ClassList {
    constructor() { this._s = new Set(); }
    add(c) { this._s.add(c); }
    remove(c) { this._s.delete(c); }
    contains(c) { return this._s.has(c); }
    toString() { return [...this._s].join(' '); }
}

class El {
    constructor(tag = 'div') {
        this.tagName = String(tag).toUpperCase();
        this.children = [];
        this._listeners = {};
        this.classList = new ClassList();
        this.attributes = {};
        this.style = {};
        this._html = '';
        this._text = '';
        this.value = '';
    }
    set innerHTML(v) { this._html = v == null ? '' : String(v); this.children = []; }
    get innerHTML() { return this._html; }
    set textContent(v) { this._text = v == null ? '' : String(v); }
    get textContent() { return this._text; }
    set href(v) { this.attributes.href = String(v); }
    get href() { return this.attributes.href || ''; }
    appendChild(n) { this.children.push(n); return n; }
    setAttribute(k, v) { this.attributes[k] = String(v); }
    getAttribute(k) { return Object.prototype.hasOwnProperty.call(this.attributes, k) ? this.attributes[k] : null; }
    addEventListener(t, fn) { (this._listeners[t] ||= []).push(fn); }
    dispatch(t, ev = {}) { (this._listeners[t] || []).forEach((fn) => fn({ preventDefault() {}, ...ev })); }
    querySelectorAll() { return []; }
}

function installHarness() {
    const els = {
        menu: new El('div'),
        view: new El('div'),
        'login-overlay': new El('div'),
        'login-form': new El('form'),
        'token-input': new El('input'),
    };

    const document = {
        getElementById: (id) => els[id] || null,
        createElement: (tag) => new El(tag),
        querySelectorAll: () => [],
        _listeners: {},
        addEventListener(t, fn) { (this._listeners[t] ||= []).push(fn); },
        dispatchEvent(ev) { (this._listeners[ev.type] || []).forEach((fn) => fn(ev)); return true; },
    };

    const win = {
        location: { hash: '#/secret' },
        _listeners: {},
        addEventListener(t, fn) { (this._listeners[t] ||= []).push(fn); },
        dispatchEvent(ev) { (this._listeners[ev.type] || []).forEach((fn) => fn(ev)); return true; },
        fireHashChange() { (this._listeners.hashchange || []).forEach((fn) => fn({ type: 'hashchange' })); },
    };

    const store = new Map([['bore_admin_token', 'test-token']]);
    const sessionStorage = {
        getItem: (k) => (store.has(k) ? store.get(k) : null),
        setItem: (k, v) => store.set(k, String(v)),
        removeItem: (k) => store.delete(k),
    };

    // Capture the single armed interval; expose a manual "tick".
    const timer = { fn: null, ms: null, cleared: 0, id: 0 };
    const setIntervalStub = (fn, ms) => { timer.fn = fn; timer.ms = ms; return ++timer.id; };
    const clearIntervalStub = () => { timer.cleared++; timer.fn = null; };

    const fetchCalls = [];
    const fetchStub = async (url) => {
        fetchCalls.push(url);
        return { status: 200, ok: true, statusText: 'OK', json: async () => [] };
    };

    globalThis.document = document;
    globalThis.window = win;
    globalThis.sessionStorage = sessionStorage;
    globalThis.setInterval = setIntervalStub;
    globalThis.clearInterval = clearIntervalStub;
    globalThis.fetch = fetchStub;
    globalThis.CustomEvent = class { constructor(type) { this.type = type; } };
    globalThis.HTMLElement = El;

    return { win, timer, fetchCalls };
}

// Let the microtask queue drain (renderPanel is async: dynamic import + fetch + render).
const flush = () => new Promise((r) => setTimeout(r, 0));

test('T-FE-POLL1: app.js arms the poller on load and auto-refresh re-fetches', async () => {
    const { win, timer, fetchCalls } = installHarness();

    // Importing app.js runs its bootstrap side effects (buildSidebar, router,
    // setupPolling) exactly as the browser would.
    await import('../../src/admin_ui/app.js');
    await flush();

    // 1. Poller armed with the secret panel's 30s interval.
    assert.equal(timer.ms, 30000, 'poller must arm with the active panel refreshMs (30s)');
    assert.equal(typeof timer.fn, 'function', 'an interval callback must be scheduled');

    // Initial render fetched the secret endpoint once.
    const initial = fetchCalls.length;
    assert.ok(
        fetchCalls.some((u) => u.includes('/admin/api/v1/secret')),
        'initial render must fetch the active endpoint',
    );

    // 2. Firing the interval must RE-FETCH (this is auto-refresh; the BUG was no re-fetch).
    timer.fn();
    await flush();
    assert.ok(fetchCalls.length > initial, 'a poll tick must re-fetch the active endpoint');
    assert.ok(
        fetchCalls.slice(initial).some((u) => u.includes('/admin/api/v1/secret')),
        'the poll tick re-fetches the SAME active endpoint',
    );

    // 3. A hashchange re-arms the poller and fetches the new endpoint.
    const beforeNav = fetchCalls.length;
    win.location.hash = '#/overview';
    win.fireHashChange();
    await flush();
    assert.equal(timer.ms, 30000, 'poller re-arms for the new panel');
    assert.ok(
        fetchCalls.slice(beforeNav).some((u) => u.includes('/admin/api/v1/summary')),
        'navigating re-fetches the new endpoint (overview → summary)',
    );
});

test('T-FE-POLL2: every data panel exposes a positive refreshMs (config exempt)', async () => {
    installHarness();
    const registry = (await import('../../src/admin_ui/registry.js')).default;
    for (const panel of registry) {
        if (panel.route === 'config') {
            assert.ok(!panel.refreshMs, 'config panel stays static (no polling)');
            continue;
        }
        assert.ok(
            typeof panel.refreshMs === 'number' && panel.refreshMs > 0,
            `panel "${panel.route}" must have refreshMs > 0 or auto-refresh silently dies`,
        );
        assert.ok(panel.endpoint, `panel "${panel.route}" must have an endpoint to poll`);
    }
});
