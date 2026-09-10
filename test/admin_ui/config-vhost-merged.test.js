/**
 * T-CFGVHOST: the config panel renders the merged vhost configuration
 * (phase 06.4 / F-6). The endpoint used to omit `default_response_headers`
 * entirely; a generic String(value) row would render the map it now carries as
 * "[object Object]", which is no more useful to an operator than the omission.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import configPanel from '../../src/admin_ui/panels/config.js';

/** Recursively collect text from a stub node tree (badges sit one level down). */
function deepText(node) {
    if (!node) return '';
    let out = node.textContent || '';
    (node.children || []).forEach(child => {
        out += deepText(child);
    });
    return out;
}

function rowFor(container, label) {
    let found = null;
    container.children.forEach(row => {
        if (row.children[0] && row.children[0].textContent === label) {
            found = row;
        }
    });
    return found;
}

const DATA = {
    control_port: 7835,
    vhost_default_request_headers: { 'x-forwarded-proto': 'https' },
    vhost_default_response_headers: {
        'strict-transport-security': 'max-age=31536000',
        'x-frame-options': 'DENY',
    },
    vhost_reservations: [
        {
            client_id: 'team-a',
            subdomain: 'app',
            headers: { 'x-tenant': 'a' },
            response_headers: { 'x-cache': 'bypass', 'x-env': 'prod' },
        },
    ],
};

test('T-CFGVHOST: default response headers render as name: value lines', async () => {
    const el = document.createElement('div');
    await configPanel.render(el, DATA);
    const container = el.children[0];

    const row = rowFor(container, 'Vhost Default Response Headers');
    assert.ok(row, 'response headers row rendered');
    const text = deepText(row.children[1]);
    assert.match(text, /x-frame-options: DENY/);
    assert.match(text, /strict-transport-security: max-age=31536000/);
    assert.doesNotMatch(text, /\[object Object\]/);
});

test('T-CFGVHOST: request headers render and reservations summarise', async () => {
    const el = document.createElement('div');
    await configPanel.render(el, DATA);
    const container = el.children[0];

    const req = rowFor(container, 'Vhost Default Request Headers');
    assert.ok(req, 'request headers row rendered');
    assert.match(deepText(req.children[1]), /x-forwarded-proto: https/);

    const res = rowFor(container, 'Vhost Reservations');
    assert.ok(res, 'reservations row rendered');
    const text = deepText(res.children[1]);
    assert.match(text, /app/);
    assert.match(text, /team-a/);
    assert.match(text, /1 req \/ 2 resp headers/);
});

test('T-CFGVHOST: an empty header map reads "none", never blank', async () => {
    const el = document.createElement('div');
    await configPanel.render(el, {
        vhost_default_response_headers: {},
        vhost_reservations: [],
    });
    const container = el.children[0];

    assert.equal(deepText(rowFor(container, 'Vhost Default Response Headers').children[1]), 'none');
    assert.equal(deepText(rowFor(container, 'Vhost Reservations').children[1]), 'none');
});

test('T-CFGVHOST: multi-line values are stacked, not laid out side by side', async () => {
    const el = document.createElement('div');
    await configPanel.render(el, DATA);
    const container = el.children[0];
    const valEl = rowFor(container, 'Vhost Default Response Headers').children[1];
    assert.match(valEl.className, /config-value-stack/);
    assert.equal(valEl.children.length, 2, 'one line per header');
});
