/**
 * T-DETAIL: detailRows formatter test — bytes, duration, boolean, null, arrays.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import { detailRows } from '../../src/admin_ui/modal.js';

test('T-DETAIL: detailRows formats _bytes fields', () => {
    const obj = {
        relay_tx_bytes: 1048576,
        relay_rx_bytes: 2097152,
    };
    const rows = detailRows(obj);
    assert.ok(rows.some(r => r.label === 'Relay Tx Bytes' && r.value.includes('MB')), 'bytes field formatted with fmtBytes');
    assert.ok(rows.some(r => r.label === 'Relay Rx Bytes' && r.value.includes('MB')), 'rx_bytes formatted');
});

test('T-DETAIL: detailRows formats _secs fields', () => {
    const obj = {
        uptime_secs: 3661,
    };
    const rows = detailRows(obj);
    assert.ok(rows.some(r => r.label === 'Uptime Secs' && r.value.includes('1h')), 'secs field formatted with fmtDuration');
});

test('T-DETAIL: detailRows formats booleans as badges', () => {
    const obj = {
        tls: true,
        udp: false,
    };
    const rows = detailRows(obj);
    const tlsRow = rows.find(r => r.label === 'Tls');
    assert.ok(tlsRow.value.classList && tlsRow.value.classList.contains('badge'), 'boolean converted to badge element');
});

test('T-DETAIL: detailRows converts null to —', () => {
    const obj = {
        notes: null,
        missing: undefined,
    };
    const rows = detailRows(obj);
    assert.ok(rows.some(r => r.label === 'Notes' && r.value === '—'), 'null → —');
    assert.ok(rows.some(r => r.label === 'Missing' && r.value === '—'), 'undefined → —');
});

test('T-DETAIL: detailRows formats arrays as joined strings', () => {
    const obj = {
        peers: ['192.168.1.1', '10.0.0.1'],
    };
    const rows = detailRows(obj);
    assert.ok(rows.some(r => r.label === 'Peers' && r.value.includes(',')), 'array joined with commas');
});

test('T-DETAIL: detailRows formats header pairs (2D array) as key: value', () => {
    const obj = {
        request_header_pairs: [
            ['Content-Type', 'application/json'],
            ['Authorization', 'Bearer token'],
        ],
    };
    const rows = detailRows(obj);
    const headerRow = rows.find(r => r.label === 'Request Header Pairs');
    assert.ok(headerRow.value.includes('Content-Type: application/json'), 'header pair formatted as key: value');
    assert.ok(headerRow.value.includes(';'), 'pairs joined with semicolon');
});

test('T-DETAIL: detailRows skips fields starting with _', () => {
    const obj = {
        public_port: 8080,
        _entry: { internal: true },
    };
    const rows = detailRows(obj);
    assert.ok(rows.some(r => r.label === 'Public Port'), 'normal field included');
    assert.ok(!rows.some(r => r.label.includes('_entry')), 'internal field skipped');
});