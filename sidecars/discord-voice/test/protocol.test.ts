import assert from 'node:assert/strict';
import test from 'node:test';
import { parseFrame } from '../src/protocol.js';

test('accepts a versioned owner-bound start request', () => {
  assert.deepEqual(parseFrame(JSON.stringify({
    version: 1, kind: 'start', requestId: 'req-1', guildId: 'guild-1',
    channelId: 'voice-1', conversationId: 'text-1', ownerId: 'owner-1', botUserId: 'bot-1',
    generation: 1,
  })), {
    version: 1, kind: 'start', requestId: 'req-1', guildId: 'guild-1',
    channelId: 'voice-1', conversationId: 'text-1', ownerId: 'owner-1', botUserId: 'bot-1',
    generation: 1,
  });
});

test('rejects unsupported versions and missing binding identity', () => {
  const good = { version: 1, kind: 'start', requestId: 'req-1', guildId: 'guild-1',
    channelId: 'voice-1', conversationId: 'text-1', ownerId: 'owner-1', botUserId: 'bot-1', generation: 1 };
  assert.throws(() => parseFrame(JSON.stringify({ ...good, version: 2 })));
  assert.throws(() => parseFrame(JSON.stringify({ ...good, ownerId: '' })));
  assert.throws(() => parseFrame(JSON.stringify({ ...good, generation: 0 })));
  assert.throws(() => parseFrame('{broken'));
});

test('rejects arbitrary speak targets and oversized text', () => {
  const speak = { version: 1, kind: 'speak', requestId: 'req-2',
    conversationId: 'text-1', generation: 1, utteranceId: 'utt-1', text: 'hello' };
  assert.deepEqual(parseFrame(JSON.stringify(speak)), speak);
  assert.throws(() => parseFrame(JSON.stringify({ ...speak, channelId: 'other-channel' })));
  assert.throws(() => parseFrame(JSON.stringify({ ...speak, text: 'a'.repeat(12001) })));
});
