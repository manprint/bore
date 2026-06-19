/**
 * T-VHOSTLEAN: Vhost table lean test — verifies header columns removed from table,
 * but full pairs still in detail modal.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import vhostPanel from '../../src/admin_ui/panels/vhost.js';
import { detailRows } from '../../src/admin_ui/modal.js';

test('T-VHOSTLEAN: vhost table omits "Request Headers" and "Response Headers" columns', async () => {
    const data = [
        {
            id: 1,
            subdomain: 'api',
            active: 5,
            carriers: 1,
            direct_stream_opens: 3,
            request_headers: ['Authorization', 'X-Custom'],
            response_headers: ['Content-Type'],
            tls: true
        }
    ];

    const el = document.createElement('div');
    await vhostPanel.render(el, data);

    const table = el.children[0];
    const thead = table.children[0];
    const headerRow = thead.children[0];

    let foundReqHeaders = false;
    let foundRespHeaders = false;
    let foundHeadersCount = false;
    for (const th of headerRow.children) {
        if (th.textContent === 'Request Headers') foundReqHeaders = true;
        if (th.textContent === 'Response Headers') foundRespHeaders = true;
        if (th.textContent.includes('Headers')) foundHeadersCount = true;
    }

    assert.ok(!foundReqHeaders, 'Request Headers column NOT in table');
    assert.ok(!foundRespHeaders, 'Response Headers column NOT in table');
    assert.ok(foundHeadersCount, 'Headers count badge column present in table');
});

test('T-VHOSTLEAN: vhost table shows header count badge in card', async () => {
    const data = [
        {
            id: 1,
            subdomain: 'api',
            active: 5,
            carriers: 1,
            direct_stream_opens: 3,
            request_headers: ['Authorization', 'X-Custom'],
            response_headers: ['Content-Type'],
            tls: true
        }
    ];

    const el = document.createElement('div');
    await vhostPanel.render(el, data);

    const table = el.children[0];
    const tbody = table.children[1];
    const row = tbody.children[0];

    // The row should have a Headers column with the badge
    assert.ok(row.children.length >= 6, 'row has expected number of cells');

    // Check the headers cell text/content
    let headersBadgeFound = false;
    for (const cell of row.children) {
        // Check both direct HTML and badge class
        if (cell._html && cell._html.includes('req') && cell._html.includes('resp')) {
            headersBadgeFound = true;
        }
        if (cell.children && cell.children.length > 0) {
            for (const child of cell.children) {
                if (child.classList && child.classList.contains('badge')) {
                    if (child.textContent.includes('req') && child.textContent.includes('resp')) {
                        headersBadgeFound = true;
                    }
                }
            }
        }
    }

    assert.ok(headersBadgeFound, 'headers count badge found in row');
});

test('T-VHOSTLEAN: vhost detail modal still shows request_header_pairs and response_header_pairs', () => {
    const vhost = {
        id: 1,
        subdomain: 'api',
        active: 5,
        carriers: 1,
        direct_stream_opens: 3,
        request_headers: ['Authorization', 'X-Custom'],
        response_headers: ['Content-Type'],
        request_header_pairs: [
            ['Authorization', 'Bearer token123'],
            ['X-Custom', 'value']
        ],
        response_header_pairs: [
            ['Content-Type', 'application/json']
        ],
        tls: true
    };

    const rows = detailRows(vhost);

    // Check that detailRows includes the full header pairs with formatted values
    let foundReqPairs = false;
    let foundRespPairs = false;

    for (const row of rows) {
        // Labels are transformed: underscores → spaces, title-cased
        // 'request_header_pairs' becomes 'Request Header Pairs'
        if (row.label.includes('Request') && row.label.includes('Header') && row.label.includes('Pairs')) {
            foundReqPairs = true;
            // The pairs should be formatted as "Authorization: Bearer token123; X-Custom: value"
            assert.ok(row.value.includes('Authorization'), 'request header key present');
            assert.ok(row.value.includes('Bearer'), 'request header value present');
        }
        if (row.label.includes('Response') && row.label.includes('Header') && row.label.includes('Pairs')) {
            foundRespPairs = true;
            assert.ok(row.value.includes('Content-Type'), 'response header pair present');
        }
    }

    assert.ok(foundReqPairs, 'request_header_pairs in detail rows');
    assert.ok(foundRespPairs, 'response_header_pairs in detail rows');
});

test('T-VHOSTLEAN: vhost table handles zero headers gracefully', async () => {
    const data = [
        {
            id: 1,
            subdomain: 'static',
            active: 2,
            carriers: 1,
            direct_stream_opens: 1,
            request_headers: [],
            response_headers: [],
            tls: false
        }
    ];

    const el = document.createElement('div');
    await vhostPanel.render(el, data);

    const table = el.children[0];
    const tbody = table.children[1];
    const row = tbody.children[0];

    // Should render the headers badge with "0 req / 0 resp"
    let foundZeroBadge = false;
    for (const cell of row.children) {
        if (cell._html && cell._html.includes('0 req') && cell._html.includes('0 resp')) {
            foundZeroBadge = true;
        }
        if (cell.children && cell.children.length > 0) {
            for (const child of cell.children) {
                if (child.textContent && child.textContent.includes('0 req') && child.textContent.includes('0 resp')) {
                    foundZeroBadge = true;
                }
            }
        }
    }

    assert.ok(foundZeroBadge, 'zero headers badge rendered correctly');
});
