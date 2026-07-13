/**
 * T-STYLE: visual regressions guarded by CSS invariants.
 */
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

test('T-STYLE-SSH: long card values wrap inside their card', async () => {
    const css = await readFile(new URL('../../src/admin_ui/style.css', import.meta.url), 'utf8');
    assert.match(css, /\.card-item-value\s*\{[^}]*overflow-wrap:\s*anywhere;/s);
});
