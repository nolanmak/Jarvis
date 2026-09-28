import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync, readdirSync } from 'node:fs';
import { PassThrough } from 'node:stream';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { VoiceConnectionStatus, type VoiceConnection } from '@discordjs/voice';
import * as prism from 'prism-media';
import { WebSocketServer } from 'ws';
import { VoiceAudio, type SpeechConfig } from '../src/speech-runtime.js';
import type { StartFrame } from '../src/protocol.js';

const fixtureRoot = fileURLToPath(new URL('../../test/fixtures/commands/', import.meta.url));
const labels = ['yes', 'no', 'up', 'down', 'left', 'right', 'go', 'stop'] as const;
const fixtures = labels.flatMap(label => readdirSync(`${fixtureRoot}/${label}`)
  .filter(name => name.endsWith('.wav')).sort()
  .map(name => ({ label, path: `${fixtureRoot}/${label}/${name}` })));

function discordPcm(wav: Buffer): Buffer {
  assert.equal(wav.toString('ascii', 0, 4), 'RIFF');
  assert.equal(wav.toString('ascii', 8, 12), 'WAVE');
  assert.equal(wav.readUInt16LE(20), 1);
  assert.equal(wav.readUInt16LE(22), 1);
  assert.equal(wav.readUInt32LE(24), 16_000);
  assert.equal(wav.readUInt16LE(34), 16);
  assert.equal(wav.toString('ascii', 36, 40), 'data');
  const samples = wav.subarray(44);
  const output = Buffer.alloc(samples.length * 6);
  for (let source = 0, target = 0; source < samples.length; source += 2) {
    const sample = samples.readInt16LE(source);
    for (let repeat = 0; repeat < 3; repeat++) {
      output.writeInt16LE(sample, target);
      output.writeInt16LE(sample, target + 2);
      target += 4;
    }
  }
  return output;
}

async function feedOpus(stream: PassThrough, pcm: Buffer): Promise<number> {
  const encoder = new prism.opus.Encoder({ rate: 48_000, channels: 2, frameSize: 960 });
  encoder.end(pcm);
  let packets = 0;
  for await (const packet of encoder) { stream.write(packet as Buffer); packets++; }
  return packets;
}

function connection(stream: PassThrough, subscriptions: string[]): VoiceConnection {
  return Object.assign(new EventEmitter(), {
    state: { status: VoiceConnectionStatus.Ready },
    subscribe: () => ({ unsubscribe() {} }),
    receiver: { subscribe: (userId: string) => {
      subscriptions.push(userId);
      return stream;
    } },
  }) as unknown as VoiceConnection;
}

test('20 fixed prerecorded commands cross Opus decode, resampling, and both STT adapters once', async () => {
  assert.equal(fixtures.length, 20);
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  try {
    for (const [index, fixture] of fixtures.entries()) {
      const provider = index % 2 ? 'elevenlabs' : 'deepgram';
      let observedBytes = 0;
      const incoming = new Promise<{ bytes: number; content: Buffer }>(resolve => {
        server.once('connection', socket => {
          let bytes = 0;
          const content: Buffer[] = [];
          let committed = false;
          socket.on('message', (data, binary) => {
            const pcm = binary ? Buffer.from(data as Buffer)
              : Buffer.from((JSON.parse(data.toString()) as { audio_base_64: string }).audio_base_64, 'base64');
            bytes += pcm.length;
            observedBytes = bytes;
            content.push(pcm);
            if (bytes < 8_000 || committed) return;
            committed = true;
            if (provider === 'deepgram') {
              socket.send(JSON.stringify({ type: 'TurnInfo', event: 'Update',
                turn_index: 0, transcript: 'uncommitted partial' }));
              socket.send(JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn',
                turn_index: 0, transcript: fixture.label.toUpperCase() }));
            } else {
              socket.send(JSON.stringify({ message_type: 'partial_transcript',
                text: 'uncommitted partial' }));
              socket.send(JSON.stringify({ message_type: 'committed_transcript',
                text: fixture.label.toUpperCase() }));
            }
            resolve({ bytes, content: Buffer.concat(content) });
          });
        });
      });
      const binding: StartFrame = { version: 1, kind: 'start', requestId: `fixture-${index}`,
        guildId: 'guild-1', channelId: 'voice-1', conversationId: 'text-1',
        ownerId: 'owner-1', botUserId: 'bot-1', generation: index + 1 };
      const config: SpeechConfig = { sttProvider: provider, ttsProvider: 'deepgram',
        sttKey: 'synthetic-test-key', ttsKey: 'synthetic-test-key',
        sttEndpoint: `ws://127.0.0.1:${address.port}` };
      const subscriptions: string[] = [];
      const receiver = new PassThrough();
      const frames: Array<Record<string, unknown>> = [];
      const audio = new VoiceAudio(connection(receiver, subscriptions), binding, config,
        frame => { frames.push(frame as Record<string, unknown>); return true; });
      try {
        await audio.start();
        assert.deepEqual(subscriptions, ['owner-1']);
        const packets = await feedOpus(receiver, discordPcm(readFileSync(fixture.path)));
        assert.ok(packets > 0, fixture.path);
        const received = await Promise.race([incoming,
          new Promise<never>((_, reject) => setTimeout(() => reject(
            new Error(`Only ${observedBytes} PCM bytes reached ${provider} for ${fixture.path}`)), 1_000))]);
        assert.ok(received.bytes >= 8_000);
        assert.ok(received.content.some(byte => byte !== 0));
        for (let attempt = 0; frames.filter(frame => frame.kind === 'transcript').length < 1 && attempt < 100; attempt++) {
          await new Promise(resolve => setTimeout(resolve, 5));
        }
        const turns = frames.filter(frame => frame.kind === 'transcript');
        assert.equal(turns.length, 1, fixture.path);
        assert.equal((turns[0]?.text as string).trim().toLowerCase(), fixture.label, fixture.path);
        assert.equal(turns[0]?.turnId, `voice:${index + 1}:0`);
      } finally {
        audio.stop();
        receiver.destroy();
      }
    }
  } finally {
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

test('intentional silence produces no committed turn and only the owner stream is subscribed', async () => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  let received = 0;
  let nonzero = 0;
  server.on('connection', socket => socket.on('message', (data, binary) => {
    const pcm = binary ? Buffer.from(data as Buffer)
      : Buffer.from((JSON.parse(data.toString()) as { audio_base_64: string }).audio_base_64, 'base64');
    received += pcm.length;
    nonzero += pcm.filter(byte => byte !== 0).length;
  }));
  const binding: StartFrame = { version: 1, kind: 'start', requestId: 'silence',
    guildId: 'guild-1', channelId: 'voice-1', conversationId: 'text-1',
    ownerId: 'owner-1', botUserId: 'bot-1', generation: 100 };
  const subscriptions: string[] = [];
  const receiver = new PassThrough();
  const frames: Array<Record<string, unknown>> = [];
  const audio = new VoiceAudio(connection(receiver, subscriptions), binding, {
    sttProvider: 'deepgram', ttsProvider: 'deepgram',
    sttKey: 'synthetic-test-key', ttsKey: 'synthetic-test-key',
    sttEndpoint: `ws://127.0.0.1:${address.port}`,
  }, frame => { frames.push(frame as Record<string, unknown>); return true; });
  try {
    await audio.start();
    await feedOpus(receiver, discordPcm(readFileSync(`${fixtureRoot}/silence.wav`)));
    for (let attempt = 0; received < 8_000 && attempt < 100; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 5));
    }
    assert.ok(received >= 8_000, 'silence must still cross the live-format audio transport');
    assert.equal(nonzero, 0);
    assert.deepEqual(subscriptions, ['owner-1']);
    assert.equal(frames.filter(frame => frame.kind === 'transcript').length, 0);
  } finally {
    audio.stop();
    receiver.destroy();
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});
