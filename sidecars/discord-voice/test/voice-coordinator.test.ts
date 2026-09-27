import assert from 'node:assert/strict';
import test from 'node:test';
import type { CreateVoiceConnectionOptions, JoinVoiceChannelOptions } from '@discordjs/voice';
import { VoiceCoordinator } from '../src/voice-coordinator.js';
import type { StartFrame } from '../src/protocol.js';

const start = (conversationId: string, generation = 1): StartFrame => ({
  version: 1, kind: 'start', requestId: `req-${generation}`, guildId: 'guild-1',
  channelId: 'voice-1', conversationId, ownerId: 'owner-1', botUserId: 'bot-1', generation,
});

test('joins an existing channel and rejects a competing conversation', () => {
  const joined: Array<CreateVoiceConnectionOptions & JoinVoiceChannelOptions> = [];
  let destroyed = 0;
  const service = new VoiceCoordinator(options => {
    joined.push(options);
    return { destroy() { destroyed++; } };
  }, () => true);
  service.start(start('text-1'));
  assert.equal(joined[0]?.guildId, 'guild-1');
  assert.equal(joined[0]?.channelId, 'voice-1');
  assert.equal(joined[0]?.selfDeaf, false);
  assert.equal(joined[0]?.selfMute, false);
  assert.throws(() => service.start(start('text-2')));
  assert.equal(joined.length, 1);
  assert.equal(service.stop('text-1', 1), true);
  assert.equal(destroyed, 1);
});

test('ignores stale stop and gateway events after rejoin', () => {
  let destroyed = 0;
  const service = new VoiceCoordinator(() => ({ destroy() { destroyed++; } }), () => true);
  service.start(start('text-1', 1));
  assert.equal(service.stop('text-1', 1), true);
  service.start(start('text-1', 2));
  assert.equal(service.stop('text-1', 1), false);
  assert.equal(destroyed, 1);
  service.onFrame({ version: 1, kind: 'voice_state', conversationId: 'text-1', generation: 1,
    guildId: 'guild-1', userId: 'owner-1', channelId: null, sessionId: 'old' });
  assert.equal(destroyed, 1);
  service.onFrame({ version: 1, kind: 'voice_state', conversationId: 'text-1', generation: 2,
    guildId: 'guild-1', userId: 'owner-1', channelId: null, sessionId: 'new' });
  assert.equal(destroyed, 2);
  assert.throws(() => service.start(start('text-1', 2)));
});

test('fifty attach and detach cycles release every voice connection', () => {
  let joined = 0;
  let destroyed = 0;
  const service = new VoiceCoordinator(() => {
    joined++;
    let closed = false;
    return { destroy() {
      assert.equal(closed, false, 'voice connection was destroyed twice');
      closed = true;
      destroyed++;
    } };
  }, () => true);
  for (let generation = 1; generation <= 50; generation++) {
    service.start(start('text-1', generation));
    assert.equal(service.status('text-1')?.generation, generation);
    assert.equal(service.stop('text-1', generation), true);
    assert.equal(service.status('text-1'), undefined);
    assert.equal(service.stop('text-1', generation), false);
  }
  assert.equal(joined, 50);
  assert.equal(destroyed, 50);
});
