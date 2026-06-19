/**
 * T-LABELS: Table header labels test — verifies "Connections" not "Active".
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import tunnelsPanel from '../../src/admin_ui/panels/tunnels.js';
import secretPanel from '../../src/admin_ui/panels/secret.js';
import vhostPanel from '../../src/admin_ui/panels/vhost.js';

test('T-LABELS: tunnels table uses "Connections" header not "Active"', async () => {
    const data = [
        {
            id: 1,
            public_port: 8000,
            peer: '192.168.1.1:50000',
            active: 2,
            uptime_secs: 3600,
            relay_tx_bytes: 1000,
            relay_rx_bytes: 2000,
            https: false,
            force_https: false,
            basic_auth: false,
            udp: false,
            carriers: 1,
            auto_reconnect: false,
            notes: ''
        }
    ];

    const el = document.createElement('div');
    await tunnelsPanel.render(el, data);

    const table = el.children[0];
    const thead = table.children[0];
    const headerRow = thead.children[0];

    let foundConnections = false;
    let foundActive = false;
    for (const th of headerRow.children) {
        if (th.textContent === 'Connections') foundConnections = true;
        if (th.textContent === 'Active') foundActive = true;
    }

    assert.ok(foundConnections, 'Connections header present in tunnels table');
    assert.ok(!foundActive, 'Active header NOT present in tunnels table');
});

test('T-LABELS: secret table uses "Connections" header not "Active"', async () => {
    const data = [
        {
            id: 1,
            role: 'Provider',
            secret_id: 'secret123',
            peer: '10.0.0.1:50001',
            active: 1,
            uptime_secs: 7200,
            relay_tx_bytes: 5000,
            relay_rx_bytes: 10000,
            udp: false,
            basic_auth: false,
            carriers: 1
        }
    ];

    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const table = el.children[0];
    const thead = table.children[0];
    const headerRow = thead.children[0];

    let foundConnections = false;
    let foundActive = false;
    for (const th of headerRow.children) {
        if (th.textContent === 'Connections') foundConnections = true;
        if (th.textContent === 'Active') foundActive = true;
    }

    assert.ok(foundConnections, 'Connections header present in secret table');
    assert.ok(!foundActive, 'Active header NOT present in secret table');
});

test('T-LABELS: vhost table uses "Connections" header not "Active"', async () => {
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

    let foundConnections = false;
    let foundActive = false;
    for (const th of headerRow.children) {
        if (th.textContent === 'Connections') foundConnections = true;
        if (th.textContent === 'Active') foundActive = true;
    }

    assert.ok(foundConnections, 'Connections header present in vhost table');
    assert.ok(!foundActive, 'Active header NOT present in vhost table');
});
