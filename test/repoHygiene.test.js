// Regression guard: a worktree's node_modules symlink was once committed as a
// tracked path. That made every deploy checkout look dirty, and the Legion
// updater stopped deploying. node_modules must never be tracked in any form.
const test = require('node:test');
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const path = require('node:path');

const root = path.join(__dirname, '..');

test('no node_modules path is tracked (directory or symlink)', () => {
  const tracked = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8' })
    .split('\0')
    .filter((p) => p.split('/').includes('node_modules'));
  assert.deepEqual(tracked, []);
});
