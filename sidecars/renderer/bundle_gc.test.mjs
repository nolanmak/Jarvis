import assert from 'node:assert/strict';
import { test } from 'node:test';

import { BUNDLE_PREFIX, UNMARKED_MAX_AGE_MS, staleBundles } from './bundle_gc.mjs';

const dir = (suffix, ownerPid, ageMs = 0) => ({ name: `${BUNDLE_PREFIX}${suffix}`, ownerPid, ageMs });
const alive = (pids) => (pid) => pids.includes(pid);

test('a bundle whose owner is dead is stale, whatever its age', () => {
  assert.deepEqual(staleBundles([dir('dead', 41, 1000)], alive([]), 7), [`${BUNDLE_PREFIX}dead`]);
});

test("another live renderer's bundle is never taken, however old", () => {
  const month = 30 * 24 * 60 * 60 * 1000;
  assert.deepEqual(staleBundles([dir('tenant', 42, month)], alive([42]), 7), []);
});

test('this process never removes its own bundle here', () => {
  assert.deepEqual(staleBundles([dir('mine', 7, 0)], alive([]), 7), []);
});

test('an unmarked bundle goes only after a week', () => {
  const dirs = [dir('old', null, UNMARKED_MAX_AGE_MS + 1), dir('recent', null, UNMARKED_MAX_AGE_MS - 1)];
  assert.deepEqual(staleBundles(dirs, alive([]), 7), [`${BUNDLE_PREFIX}old`]);
});

test('anything that is not a bundle dir is ignored', () => {
  const stranger = { name: 'systemd-private-x', ownerPid: null, ageMs: Number.MAX_SAFE_INTEGER };
  assert.deepEqual(staleBundles([stranger], alive([]), 7), []);
});
