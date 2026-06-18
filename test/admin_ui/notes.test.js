/**
 * BUG-2: short notes must render as plain text (no fake clickable link); long
 * notes get the clickable affordance + working expand/collapse toggle.
 */
import './dom-stub.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { notesCell } from '../../src/admin_ui/ui.js';

test('short notes are plain text, not a clickable link', () => {
    const cell = notesCell('superdufs lenovo lavoro 5353', 40);
    assert.ok(cell.classList.contains('notes-plain'));
    assert.ok(!cell.classList.contains('notes-cell'), 'no clickable class');
    assert.ok(!cell.hasListener('click'), 'no click handler on short notes');
    assert.equal(cell.textContent, 'superdufs lenovo lavoro 5353');
});

test('empty notes render empty plain text', () => {
    const cell = notesCell(null, 40);
    assert.ok(cell.classList.contains('notes-plain'));
    assert.equal(cell.textContent, '');
});

test('long notes are clickable and toggle expand/collapse', () => {
    const long = 'x'.repeat(60);
    const cell = notesCell(long, 40);
    assert.ok(cell.classList.contains('notes-cell'));
    assert.ok(cell.hasListener('click'), 'long notes attach a click handler');
    assert.ok(cell.innerHTML.endsWith('...'), 'starts truncated');

    cell.dispatch('click');
    assert.ok(cell.classList.contains('expanded'));
    assert.equal(cell.innerHTML, long, 'expands to full text');

    cell.dispatch('click');
    assert.ok(!cell.classList.contains('expanded'));
    assert.ok(cell.innerHTML.endsWith('...'), 'collapses back');
});
