/**
 * T-CFGNULL: Config panel null-label rendering test.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import configPanel from '../../src/admin_ui/panels/config.js';

test('T-CFGNULL: null udp_socket_send_buffer renders as "auto (OS default)"', async () => {
    const data = {
        udp_socket_send_buffer: null,
        control_port: 7835,
    };

    const el = document.createElement('div');
    await configPanel.render(el, data);

    const container = el.children[0];
    assert.ok(container, 'config-container rendered');

    // Find the send buffer row
    let foundAutoLabel = false;
    container.children.forEach(row => {
        if (row.children[0].textContent === 'udp_socket_send_buffer') {
            const valueText = row.children[1].textContent;
            assert.equal(valueText, 'auto (OS default)', 'null buffer shows friendly label');
            foundAutoLabel = true;
        }
    });
    assert.ok(foundAutoLabel, 'udp_socket_send_buffer row found and checked');
});

test('T-CFGNULL: null udp_socket_recv_buffer renders as "auto (OS default)"', async () => {
    const data = {
        udp_socket_recv_buffer: null,
        control_port: 7835,
    };

    const el = document.createElement('div');
    await configPanel.render(el, data);

    const container = el.children[0];
    let foundAutoLabel = false;
    container.children.forEach(row => {
        if (row.children[0].textContent === 'udp_socket_recv_buffer') {
            const valueText = row.children[1].textContent;
            assert.equal(valueText, 'auto (OS default)', 'null recv buffer shows friendly label');
            foundAutoLabel = true;
        }
    });
    assert.ok(foundAutoLabel, 'udp_socket_recv_buffer row found and checked');
});

test('T-CFGNULL: numeric buffer values render humanized in MiB', async () => {
    const data = {
        udp_socket_send_buffer: 16777216,
        control_port: 7835,
    };

    const el = document.createElement('div');
    await configPanel.render(el, data);

    const container = el.children[0];
    let foundNumeric = false;
    container.children.forEach(row => {
        if (row.children[0].textContent === 'udp_socket_send_buffer') {
            const valueText = row.children[1].textContent;
            // config.js humanizes socket buffers to MiB (to match the sibling
            // udp_*_window values), so 16777216 → "16 MiB".
            assert.equal(valueText, '16 MiB', 'numeric buffer humanized to MiB');
            foundNumeric = true;
        }
    });
    assert.ok(foundNumeric, 'numeric buffer row found and checked');
});