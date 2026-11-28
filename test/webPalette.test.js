const test = require('node:test');
const assert = require('node:assert/strict');
const { rankPalette } = require('../dist/web/palette.logic');

const actions = [
  { id: 'new-thread', label: 'New thread', href: '/?new=1' },
  { id: 'approvals', label: 'Open approvals', href: '/#approvals' },
  { id: 'legacy-memory', label: 'Legacy: Memory', href: '/legacy/memory' },
];
const threads = [
  { id: 't-old', title: 'Newsletter draft', updatedAt: 1000 },
  { id: 't-new', title: 'Trip to Philly', updatedAt: 3000 },
  { id: 't-mid', title: 'Open questions list', updatedAt: 2000 },
];
const ids = (entries) => entries.map((e) => e.id);

test('empty query lists actions in order, then threads most recent first', () => {
  assert.deepEqual(ids(rankPalette('', { actions, threads })),
    ['new-thread', 'approvals', 'legacy-memory', 't-new', 't-mid', 't-old']);
  assert.deepEqual(ids(rankPalette('   ', { actions, threads })), ids(rankPalette('', { actions, threads })));
});

test('entries carry kind and label; actions carry their href', () => {
  const [first, , , thread] = rankPalette('', { actions, threads });
  assert.deepEqual(first, { kind: 'action', id: 'new-thread', label: 'New thread', href: '/?new=1' });
  assert.deepEqual(thread, { kind: 'thread', id: 't-new', label: 'Trip to Philly' });
});

test('prefix beats word-start beats subsequence; non-matches are dropped', () => {
  assert.deepEqual(ids(rankPalette('new', { actions, threads })), ['new-thread', 't-old']);
  // "op": prefix of "Open approvals" and "Open questions"; a subsequence of "Trip tO Philly".
  assert.deepEqual(ids(rankPalette('op', { actions, threads })), ['approvals', 't-mid', 't-new']);
  // "mem": word-start in "Legacy: Memory".
  assert.deepEqual(ids(rankPalette('mem', { actions, threads })), ['legacy-memory']);
  // "phi" is a word start; "tp" is only a subsequence.
  assert.deepEqual(ids(rankPalette('phi', { actions, threads })), ['t-new']);
  assert.deepEqual(ids(rankPalette('tp', { actions, threads })), ['t-new']);
  assert.deepEqual(ids(rankPalette('zzz', { actions, threads })), []);
});

test('tier order is strict across kinds, actions first on ties, then recency', () => {
  const t = [
    { id: 'a', title: 'Draft reply', updatedAt: 1 },
    { id: 'b', title: 'Old draft', updatedAt: 5 },
    { id: 'c', title: 'Drafts again', updatedAt: 9 },
    { id: 'd', title: 'D r a f t', updatedAt: 10 },
  ];
  const acts = [{ id: 'x', label: 'Draft email', href: '/x' }, { id: 'y', label: 'My draft', href: '/y' }];
  assert.deepEqual(ids(rankPalette('draft', { actions: acts, threads: t })),
    ['x', 'c', 'a', 'y', 'b', 'd']);
});

test('matching is case-insensitive and whitespace-trimmed', () => {
  assert.deepEqual(ids(rankPalette('  OPEN ', { actions, threads })), ['approvals', 't-mid']);
});

test('pure and deterministic: inputs untouched, equal recency broken by id', () => {
  const t = [
    { id: 'z', title: 'Same', updatedAt: 1 },
    { id: 'm', title: 'Same', updatedAt: 1 },
  ];
  const snapshot = JSON.stringify({ actions, t });
  assert.deepEqual(ids(rankPalette('same', { actions, threads: t })), ['m', 'z']);
  assert.deepEqual(ids(rankPalette('', { actions: [], threads: t })), ['m', 'z']);
  assert.equal(JSON.stringify({ actions, t }), snapshot);
});

test('limit caps the result', () => {
  assert.equal(rankPalette('', { actions, threads }, { limit: 2 }).length, 2);
  assert.equal(rankPalette('', { actions, threads }).length, 6);
});
