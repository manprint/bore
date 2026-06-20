/**
 * T-SECRETDETAIL: secret panel detail modal test — verifies that row clicks
 * open a modal with all entry fields visible (including secret-specific fields
 * like carriers, auto_reconnect, local_proxy_port, nat_udp_preferred_port).
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import secretPanel from '../../src/admin_ui/panels/secret.js';
import { closeModal } from '../../src/admin_ui/modal.js';

test('T-SECRETDETAIL: row click opens modal with entry details', async () => {
    const data = [
        {
            id: 1,
            secret_id: 'test-secret',
            role: 'secretconsumer',
            peer: '10.0.0.1:50000',
            local_proxy_port: 5432,
            carriers: 4,
            udp: true,
            auto_reconnect: true,
            basic_auth: false,
            webserver_log: false,
            nat_udp_preferred_port: 443,
            active: 3,
            uptime_secs: 7200,
            relay_tx_bytes: 1048576,
            relay_rx_bytes: 2097152,
            notes: 'Test consumer'
        }
    ];

    closeModal();
    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const table = body.children[0].children[1];
    const tbody = table.children[1];
    const row = tbody.children[0];

    row.dispatch('click');

    const overlay = document.body.children[0];
    assert.ok(overlay && overlay.classList.contains('modal-overlay'), 'modal overlay created');

    const modal = overlay.children[0];
    const modalBody = modal.children[1];
    const dl = modalBody.children[0];

    const rows = dl.children.length / 2;
    assert.ok(rows > 0, 'detail rows rendered in modal');

    const dlHtml = dl.innerHTML;
    assert.ok(dlHtml.includes('Carriers'), 'carriers field in modal');
    assert.ok(dlHtml.includes('Auto Reconnect'), 'auto_reconnect field in modal');
    assert.ok(dlHtml.includes('Local Proxy Port'), 'local_proxy_port field in modal');
    assert.ok(dlHtml.includes('Nat Udp Preferred Port'), 'nat_udp_preferred_port field in modal');

    closeModal();
});

test('T-SECRETDETAIL: modal shows entry notes', async () => {
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
            active: 1,
            uptime_secs: 600,
            relay_tx_bytes: 100000,
            relay_rx_bytes: 200000,
            notes: 'Important provider notes'
        }
    ];

    closeModal();
    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const table = body.children[0].children[1];
    const tbody = table.children[1];
    const row = tbody.children[0];

    row.dispatch('click');

    const overlay = document.body.children[0];
    const modal = overlay.children[0];
    const modalBody = modal.children[1];
    const dlHtml = modalBody.innerHTML;

    assert.ok(dlHtml.includes('Important provider notes'), 'notes field visible in modal');

    closeModal();
});

test('T-SECRETDETAIL: modal title includes secret_id and peer', async () => {
    const data = [
        {
            id: 1,
            secret_id: 'my-secret',
            role: 'secretconsumer',
            peer: '10.99.0.5:5000',
            local_proxy_port: 5432,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
            basic_auth: false,
            webserver_log: false,
            active: 0,
            uptime_secs: 0,
            relay_tx_bytes: 0,
            relay_rx_bytes: 0,
            notes: null
        }
    ];

    closeModal();
    const el = document.createElement('div');
    await secretPanel.render(el, data);

    const container = el.children[0];
    const card = container.children[0];
    const body = card.children[1];
    const table = body.children[0].children[1];
    const tbody = table.children[1];
    const row = tbody.children[0];

    row.dispatch('click');

    const overlay = document.body.children[0];
    const modal = overlay.children[0];
    const header = modal.children[0];
    const titleEl = header.children[0];

    assert.ok(titleEl.textContent.includes('my-secret'), 'secret_id in modal title');
    assert.ok(titleEl.textContent.includes('10.99.0.5:5000'), 'peer in modal title');

    closeModal();
});
