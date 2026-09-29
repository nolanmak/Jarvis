import assert from 'node:assert/strict';
import test from 'node:test';
import type { DiscordGatewayAdapterLibraryMethods } from '@discordjs/voice';
import { GatewayBridge } from '../src/gateway-bridge.js';

test('sends only Discord voice-state payloads to the gateway', () => {
  const sent: unknown[] = [];
  const bridge = new GatewayBridge('guild-1', 'text-1', 'bot-1', 2, payload => { sent.push(payload); return true; });
  const adapter = bridge.create({ destroy() {}, onVoiceServerUpdate() {}, onVoiceStateUpdate() {} });
  assert.equal(adapter.sendPayload({ op: 4, d: { guild_id: 'guild-1', channel_id: 'voice-1', self_deaf: false, self_mute: false } }), true);
  assert.equal(adapter.sendPayload({ op: 2, d: {} }), false);
  assert.equal(adapter.sendPayload({ op: 4, d: { guild_id: 'guild-other', channel_id: 'voice-2' } }), false);
  assert.equal(sent.length, 1);
});

test('forwards only current-generation gateway events to voice library', () => {
  const observed: string[] = [];
  const methods = {
    destroy() { observed.push('destroy'); },
    onVoiceStateUpdate() { observed.push('state'); },
    onVoiceServerUpdate() { observed.push('server'); },
  } satisfies DiscordGatewayAdapterLibraryMethods;
  const bridge = new GatewayBridge('guild-1', 'text-1', 'bot-1', 2, () => true);
  bridge.create(methods);
  bridge.onFrame({ version: 1, kind: 'voice_state', conversationId: 'text-1', generation: 2,
    guildId: 'guild-1', userId: 'someone-else', channelId: 'voice-1', sessionId: 'other-1' });
  bridge.onFrame({ version: 1, kind: 'voice_state', conversationId: 'text-1', generation: 1,
    guildId: 'guild-1', userId: 'bot-1', channelId: 'voice-1', sessionId: 'native-1' });
  bridge.onFrame({ version: 1, kind: 'voice_state', conversationId: 'text-1', generation: 2,
    guildId: 'guild-1', userId: 'bot-1', channelId: 'voice-1', sessionId: 'native-1' });
  bridge.onFrame({ version: 1, kind: 'voice_server', conversationId: 'text-1', generation: 2,
    guildId: 'guild-1', endpoint: 'voice.example.invalid', token: 'test-token' });
  bridge.destroy();
  assert.deepEqual(observed, ['state', 'server', 'destroy']);
});
