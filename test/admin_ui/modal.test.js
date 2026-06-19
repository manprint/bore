/**
 * T-MODAL: Modal component test — open, close, overlay interaction.
 * These are high-level smoke tests of the modal API (structure creation).
 * Full interactive tests (Esc, click-outside) are manual/e2e since they
 * require real event dispatch and DOM focus.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import './dom-stub.js';
import { openModal, closeModal } from '../../src/admin_ui/modal.js';

test('T-MODAL: openModal creates overlay with title and rows', () => {
    // Note: we test the basic structure here; full event dispatch
    // is tested in e2e/manual since the dom-stub has limited event support.

    const rows = [
        { label: 'Port', value: '8080' },
        { label: 'Status', value: 'active' },
    ];

    // Reset global state
    closeModal();

    openModal('Test Modal', rows);

    // Check overlay exists in document.body
    assert.ok(document.body.children.length > 0, 'overlay appended to body');
    const overlay = document.body.children[0];
    assert.ok(overlay.classList.contains('modal-overlay'), 'overlay has correct class');

    // Check modal structure
    const modal = overlay.children[0];
    assert.ok(modal.classList.contains('modal'), 'modal element present');

    const header = modal.children[0];
    assert.ok(header.classList.contains('modal-header'), 'header present');
    const titleEl = header.children[0];
    assert.equal(titleEl.textContent, 'Test Modal', 'title rendered');

    // Body contains dl with rows
    const body = modal.children[1];
    assert.ok(body.classList.contains('modal-body'), 'body present');
    assert.ok(body.children[0], 'dl element created for rows');

    closeModal();
});

test('T-MODAL: openModal again closes previous (single overlay invariant)', () => {
    // Reset global state
    closeModal();

    openModal('Modal 1', [{ label: 'A', value: 'B' }]);
    const firstOverlay = document.body.children[0];
    assert.ok(firstOverlay, 'first modal open');

    openModal('Modal 2', [{ label: 'C', value: 'D' }]);
    assert.ok(document.body.children.length > 0, 'second modal open (replaces first)');
    const modal = document.body.children[0].children[0];
    const titleEl = modal.children[0].children[0];
    assert.equal(titleEl.textContent, 'Modal 2', 'second modal title rendered');

    closeModal();
    assert.equal(document.body.children.length, 0, 'closeModal removes overlay from body');
});