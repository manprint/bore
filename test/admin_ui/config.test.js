/**
 * T-CFGNULL: Config panel null-label rendering test.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
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

test('T-CFG-SSH: config panel handles SSH keys without error', async () => {
    const data = {
        control_port: 7835,
        version: '0.5.0',
        ssh_gateway: true,
        ssh_port: 2222,
        ssh_advertise_address: 'ssh.example.com',
        ssh_auth_pubkey: true,
        ssh_auth_password: false
    };

    const el = document.createElement('div');
    await configPanel.render(el, data);

    // Config should render successfully with SSH keys
    assert.ok(el.children.length > 0, 'config panel renders');
});

test('T-CFG-SSH: SSH section keeps labels and values paired', async () => {
    const data = {
        control_port: 7835,
        ssh_gateway: true,
        ssh_auth_pubkey: true,
        ssh_auth_password: false
    };

    const el = document.createElement('div');
    await configPanel.render(el, data);

    const container = el.children[0];
    const headerIndex = container.children.findIndex(
        child => child.classList.contains('config-header')
    );
    assert.equal(headerIndex, 1, 'SSH heading follows non-SSH rows');
    assert.equal(container.children[headerIndex].textContent, 'SSH Gateway');

    const sshRows = container.children.slice(headerIndex + 1);
    const values = new Map(sshRows.map(row => [
        row.children[0].textContent,
        row.children[1].innerHTML,
    ]));
    assert.equal(values.get('SSH Gateway'), 'Yes');
    assert.equal(values.get('Public-Key Auth'), 'Yes');
    assert.equal(values.get('Password Auth'), 'No');
});

test('T-CFG-SSH: SSH heading spans both config columns', async () => {
    const css = await readFile(new URL('../../src/admin_ui/style.css', import.meta.url), 'utf8');
    assert.match(css, /\.config-header\s*\{[^}]*grid-column:\s*1\s*\/\s*-1;/s);
});

test('T-CFG-LAYOUT: config cells share a uniform grid row height', async () => {
    const css = await readFile(new URL('../../src/admin_ui/style.css', import.meta.url), 'utf8');

    assert.match(css, /\.config-container\s*\{[^}]*grid-auto-rows:\s*minmax\(3rem,\s*auto\);/s);
    assert.match(css, /\.config-key,\s*\.config-value\s*\{[^}]*align-self:\s*stretch;[^}]*align-items:\s*center;/s);
});
