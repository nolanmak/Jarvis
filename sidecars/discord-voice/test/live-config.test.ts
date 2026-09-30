import assert from 'node:assert/strict';
import test from 'node:test';
import { parseLiveArgs } from '../src/live-config.js';

const valid = ['--guild-id', '123456789012345678', '--text-channel-id', '223456789012345678',
  '--voice-channel-id', '323456789012345678', '--agent', 'codex', '--run-id', 'disposable-01'];

test('live verification requires explicit test targets and disposable run identity', () => {
  assert.deepEqual(parseLiveArgs([...valid, '--preflight-only']), {
    guildId: '123456789012345678', textChannelId: '223456789012345678',
    voiceChannelId: '323456789012345678', agent: 'codex', runId: 'disposable-01',
    output: undefined, preflightOnly: true,
  });
  assert.throws(() => parseLiveArgs(valid.slice(0, 6)), /Choose --agent/);
  assert.throws(() => parseLiveArgs([...valid, '--guild-id', '999']), /duplicate/);
  assert.throws(() => parseLiveArgs([...valid, '--output']), /Missing value/);
  assert.throws(() => parseLiveArgs([...valid.slice(0, 9), 'personal session']), /disposable/);
});
