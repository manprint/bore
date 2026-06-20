/**
 * T-SECRETGROUP: secret panel card-grouping test — verifies the grouped
 * card structure (header with secret_id + role partitioning).
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import secretPanel from '../../src/admin_ui/panels/secret.js';

test('T-SECRETGROUP: renders 2 cards for 2 secret_ids', async () => {
    const data = [
        {
            id: 1,
            secret_id: 'my-secret-1',
            role: 'secretprovider',
            peer: '10.0.0.1:50000',
            local_host: 'localhost',
            local_port: 8080,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 5,
            uptime_secs: 3600,
            relay_tx_bytes: 1024000,
            relay_rx_bytes: 2048000,
            notes: 'Provider A'
        },
        {
            id: 2,
            secret_id: 'my-secret-1',
            role: 'secretconsumer',
            peer: '10.0.0.2:50001',
            local_proxy_port: 5432,
            carriers: 4,
            udp: true,
            auto_reconnect: true,
            basic_auth: false,
            webserver_log: false,
            nat_udp_preferred_port: 443,
            active: 3,
            uptime_secs: 1800,
            relay_tx_bytes: 512000,
            relay_rx_bytes: 1024000,
            notes: 'Consumer A'
        },
        {
            id: 3,
            secret_id: 'my-secret-2',
            role: 'secretprovider',
            peer: '10.0.0.3:50002',
            local_host: 'localhost',
            local_port: 9090,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 2,
            uptime_secs: 7200,
            relay_tx_bytes: 2048000,
            relay_rx_bytes: 4096000,
            notes: 'Provider B'
        },
        {
            id: 4,
            secret_id: 'my-secret-2',
            role: 'secretconsumer',
            peer: '10.0.0.4:50003',
            local_proxy_port: 5433,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 1,
            uptime_secs: 900,
            relay_tx_bytes: 256000,
            relay_rx_bytes: 512000,
            notes: 'Consumer B'
        }
    ];

    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    assert.ok(container, 'container rendered');

    const cards = container.children;
    assert.equal(cards.length, 2, '2 cards rendered (one per secret_id)');

    // Card 1: my-secret-1
    const card1 = cards[0];
    assert.ok(card1.classList.contains('secret-card'), 'card has secret-card class');
    const header1 = card1.children[0];
    assert.ok(header1.innerHTML.includes('my-secret-1'), 'card 1 header shows secret_id');

    const body1 = card1.children[1];
    const sections1 = body1.children;
    assert.ok(sections1.length >= 2, 'card 1 has provider and consumer sections');
});

test('T-SECRETGROUP: consumer row shows local_proxy_port', async () => {
    const data = [
        {
            id: 1,
            secret_id: 'test-secret',
            role: 'secretconsumer',
            peer: '192.168.1.100:50000',
            local_proxy_port: 5432,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 1,
            uptime_secs: 600,
            relay_tx_bytes: 100000,
            relay_rx_bytes: 200000,
            notes: null
        }
    ];

    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const table = body.children[0].children[1];
    const tbody = table.children[1];
    const row = tbody.children[0];

    const localCell = row.children[1];
    assert.ok(localCell.textContent.includes('5432'), 'consumer row shows local_proxy_port');
});

test('T-SECRETGROUP: provider row shows local_host:local_port', async () => {
    const data = [
        {
            id: 1,
            secret_id: 'test-secret',
            role: 'secretprovider',
            peer: '192.168.1.100:50000',
            local_host: 'localhost',
            local_port: 8080,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 2,
            uptime_secs: 1200,
            relay_tx_bytes: 500000,
            relay_rx_bytes: 1000000,
            notes: null
        }
    ];

    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const table = body.children[0].children[1];
    const tbody = table.children[1];
    const row = tbody.children[0];

    const localCell = row.children[1];
    assert.ok(localCell.textContent.includes('localhost:8080'), 'provider row shows local_host:local_port');
});

test('T-SECRETGROUP: null secret_id falls back to #<id>', async () => {
    const data = [
        {
            id: 99,
            secret_id: null,
            role: 'secretprovider',
            peer: '10.0.0.1:50000',
            local_host: '127.0.0.1',
            local_port: 3000,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 1,
            uptime_secs: 300,
            relay_tx_bytes: 10000,
            relay_rx_bytes: 20000,
            notes: null
        }
    ];

    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const header = card.children[0];
    assert.ok(header.innerHTML.includes('#99'), 'null secret_id shows fallback #<id>');
});

test('T-SECRETGROUP: badges render for entry flags', async () => {
    const data = [
        {
            id: 1,
            secret_id: 'test',
            role: 'secretconsumer',
            peer: '10.0.0.1:50000',
            local_proxy_port: 5432,
            carriers: 4,
            udp: true,
            auto_reconnect: true,
            basic_auth: false,
            webserver_log: false,
            nat_udp_preferred_port: 443,
            active: 1,
            uptime_secs: 600,
            relay_tx_bytes: 100000,
            relay_rx_bytes: 200000,
            notes: null
        }
    ];

    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const table = body.children[0].children[1];
    const tbody = table.children[1];
    const row = tbody.children[0];
    const flagsCell = row.children[2];

    const flagsHtml = flagsCell.innerHTML;
    assert.ok(flagsHtml.includes('x4 carriers'), 'carriers badge present');
    assert.ok(flagsHtml.includes('UDP'), 'UDP badge present');
    assert.ok(flagsHtml.includes('Auto-reconnect'), 'Auto-reconnect badge present');
    assert.ok(flagsHtml.includes('NAT:443'), 'NAT port badge present');
});
