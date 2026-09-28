import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import test from 'node:test';
import { VoiceConnectionStatus, type VoiceConnection } from '@discordjs/voice';
import { WebSocketServer } from 'ws';
import { VoiceAudio, type SpeechConfig } from '../src/speech-runtime.js';
import type { StartFrame } from '../src/protocol.js';

const binding: StartFrame = { version: 1, kind: 'start', requestId: 'request-1',
  guildId: 'guild-1', channelId: 'voice-1', conversationId: 'text-1',
  ownerId: 'owner-1', botUserId: 'bot-1', generation: 7 };

function fakeConnection(): VoiceConnection {
  return Object.assign(new EventEmitter(), {
    state: { status: VoiceConnectionStatus.Ready },
    subscribe: () => ({ unsubscribe() {} }),
    receiver: { subscribe: () => new PassThrough() },
  }) as unknown as VoiceConnection;
}

test('STT disconnect reconnects without replaying or suppressing a later turn', async () => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  let connections = 0;
  server.on('connection', socket => {
    connections++;
    socket.send(JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn', turn_index: 0,
      transcript: connections === 1 ? 'first phrase' : 'second phrase' }));
    if (connections === 1) setTimeout(() => socket.close(), 10);
  });
  const config: SpeechConfig = { sttProvider: 'deepgram', ttsProvider: 'deepgram',
    sttKey: 'synthetic-key', ttsKey: 'synthetic-key',
    sttEndpoint: `ws://127.0.0.1:${address.port}`, sttRetryDelays: [10, 20, 40] };
  const frames: Array<Record<string, unknown>> = [];
  const audio = new VoiceAudio(fakeConnection(), binding, config, frame => {
    frames.push(frame as Record<string, unknown>);
    return true;
  });
  try {
    await audio.start();
    await new Promise<void>((resolve, reject) => {
      const interval = setInterval(() => {
        if (frames.filter(frame => frame.kind === 'transcript').length === 2) {
          clearInterval(interval);
          clearTimeout(deadline);
          resolve();
        }
      }, 5);
      const deadline = setTimeout(() => {
        clearInterval(interval);
        reject(new Error(`STT reconnect timed out: connections=${connections} frames=${JSON.stringify(frames)}`));
      }, 500);
    });
    assert.equal(connections, 2);
    assert.deepEqual(frames.filter(frame => frame.kind === 'transcript').map(frame => frame.text),
      ['first phrase', 'second phrase']);
    const turnIds = frames.filter(frame => frame.kind === 'transcript').map(frame => frame.turnId);
    assert.equal(new Set(turnIds).size, 2);
    assert.ok(frames.filter(frame => frame.kind === 'transcript')
      .every(frame => typeof frame.committedAtMs === 'number'));
    assert.equal(audio.status, 'listening');
  } finally {
    audio.stop();
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

test('three failed STT reconnect attempts end with one visible failure and stopped audio', async () => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  await new Promise<void>(resolve => server.close(() => resolve()));
  const frames: Array<Record<string, unknown>> = [];
  const audio = new VoiceAudio(fakeConnection(), binding, {
    sttProvider: 'deepgram', ttsProvider: 'deepgram',
    sttKey: 'synthetic-key', ttsKey: 'synthetic-key',
    sttEndpoint: `ws://127.0.0.1:${address.port}`, sttRetryDelays: [10, 20, 40],
  }, frame => { frames.push(frame as Record<string, unknown>); return true; });
  await audio.start();
  assert.equal(audio.status, 'stopped');
  assert.equal(frames.filter(frame => frame.kind === 'audio_failure').length, 1);
  assert.ok(frames.some(frame => frame.kind === 'audio_status' && frame.state === 'reconnecting'));
  assert.ok(frames.some(frame => frame.kind === 'audio_status' && frame.state === 'stopped'));
  assert.equal(frames.filter(frame => frame.kind === 'transcript').length, 0);
});

test('interrupt emits the affected receipt for the original text mirror', async () => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  const frames: Array<Record<string, unknown>> = [];
  const audio = new VoiceAudio(fakeConnection(), binding, {
    sttProvider: 'deepgram', ttsProvider: 'deepgram',
    sttKey: 'synthetic-key', ttsKey: 'synthetic-key',
    sttEndpoint: `ws://127.0.0.1:${address.port}`,
  }, frame => { frames.push(frame as Record<string, unknown>); return true; });
  try {
    await audio.start();
    audio.speak('turn-1:answer', 'synthetic spoken answer');
    audio.interrupt();
    assert.equal(audio.speechStatus('turn-1:answer')?.status, 'interrupted');
    assert.deepEqual(frames.filter(frame => frame.kind === 'speech_interrupted')
      .map(frame => frame.utteranceId), ['turn-1:answer']);
    assert.equal(frames.find(frame => frame.kind === 'speech_interrupted')?.partialAudioPlayed, false);
    audio.interrupt();
    assert.equal(frames.filter(frame => frame.kind === 'speech_interrupted').length, 1);
  } finally {
    audio.stop();
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

test('a 120-second owner utterance stops at the configured cap without submitting a turn', async t => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  let peer: import('ws').WebSocket | undefined;
  server.on('connection', socket => { peer = socket; });
  const frames: Array<Record<string, unknown>> = [];
  const audio = new VoiceAudio(fakeConnection(), binding, {
    sttProvider: 'deepgram', ttsProvider: 'deepgram',
    sttKey: 'synthetic-key', ttsKey: 'synthetic-key',
    sttEndpoint: `ws://127.0.0.1:${address.port}`,
  }, frame => { frames.push(frame as Record<string, unknown>); return true; });
  try {
    await audio.start();
    assert.ok(peer);
    const realSetTimeout = globalThis.setTimeout;
    t.mock.timers.enable({ apis: ['setTimeout'] });
    peer.send(JSON.stringify({ type: 'TurnInfo', event: 'StartOfTurn', turn_index: 0 }));
    await new Promise<void>(resolve => realSetTimeout(resolve, 20));
    t.mock.timers.tick(119_999);
    assert.equal(audio.status, 'listening');
    t.mock.timers.tick(1);
    assert.equal(audio.status, 'stopped');
    assert.equal(frames.filter(frame => frame.kind === 'audio_failure').length, 1);
    assert.equal(frames.filter(frame => frame.kind === 'transcript').length, 0);
  } finally {
    t.mock.timers.reset();
    audio.stop();
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

test('duplicate speech-start events cannot leave a stale 120-second cap after commit', async t => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  let peer: import('ws').WebSocket | undefined;
  server.on('connection', socket => { peer = socket; });
  const frames: Array<Record<string, unknown>> = [];
  const audio = new VoiceAudio(fakeConnection(), binding, {
    sttProvider: 'deepgram', ttsProvider: 'deepgram',
    sttKey: 'synthetic-key', ttsKey: 'synthetic-key',
    sttEndpoint: `ws://127.0.0.1:${address.port}`,
  }, frame => { frames.push(frame as Record<string, unknown>); return true; });
  const realSetTimeout = globalThis.setTimeout;
  try {
    await audio.start();
    assert.ok(peer);
    t.mock.timers.enable({ apis: ['setTimeout'] });
    peer.send(JSON.stringify({ type: 'TurnInfo', event: 'StartOfTurn', turn_index: 0 }));
    await new Promise<void>(resolve => realSetTimeout(resolve, 20));
    t.mock.timers.tick(60_000);
    peer.send(JSON.stringify({ type: 'TurnInfo', event: 'StartOfTurn', turn_index: 0 }));
    await new Promise<void>(resolve => realSetTimeout(resolve, 20));
    peer.send(JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn', turn_index: 0,
      transcript: 'finished before cap' }));
    await new Promise<void>(resolve => realSetTimeout(resolve, 20));
    assert.equal(frames.filter(frame => frame.kind === 'transcript').length, 1);
    t.mock.timers.tick(120_000);
    assert.equal(audio.status, 'listening');
    assert.equal(frames.filter(frame => frame.kind === 'audio_failure').length, 0);
  } finally {
    t.mock.timers.reset();
    audio.stop();
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

for (const trigger of ['owner speech-start', 'voice interrupt command'] as const) {
  test(`${trigger} cancels active TTS and marks its receipt within 250 ms`, async () => {
    const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
    await new Promise<void>(resolve => server.once('listening', resolve));
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('expected TCP address');
    let sttPeer: import('ws').WebSocket | undefined;
    let ttsPeer: import('ws').WebSocket | undefined;
    let finishFirst!: () => void;
    const firstAudio = new Promise<void>(resolve => { finishFirst = resolve; });
    let finishClosed!: () => void;
    const ttsClosed = new Promise<void>(resolve => { finishClosed = resolve; });
    server.on('connection', (socket, request) => {
      if (request.url?.startsWith('/v2/listen')) { sttPeer = socket; return; }
      if (!request.url?.startsWith('/v1/speak')) throw new Error('Unexpected provider path');
      ttsPeer = socket;
      socket.once('close', finishClosed);
      socket.on('message', data => {
        if ((JSON.parse(data.toString()) as { type: string }).type !== 'Flush') return;
        socket.send(Buffer.alloc(48_000));
        finishFirst();
      });
    });
    const frames: Array<Record<string, unknown>> = [];
    const audio = new VoiceAudio(fakeConnection(), binding, {
      sttProvider: 'deepgram', ttsProvider: 'deepgram',
      sttKey: 'synthetic-key', ttsKey: 'synthetic-key',
      sttEndpoint: `ws://127.0.0.1:${address.port}`,
      ttsEndpoint: `ws://127.0.0.1:${address.port}`,
    }, frame => { frames.push(frame as Record<string, unknown>); return true; });
    try {
      await audio.start();
      audio.speak('active-answer', 'first chunk then interruption');
      await Promise.race([firstAudio, new Promise<never>((_, reject) => setTimeout(
        () => reject(new Error('TTS did not start')), 1_000))]);
      assert.equal(audio.speechStatus('active-answer')?.status, 'playing');
      const began = performance.now();
      if (trigger === 'owner speech-start') {
        assert.ok(sttPeer);
        sttPeer.send(JSON.stringify({ type: 'TurnInfo', event: 'StartOfTurn', turn_index: 0 }));
      } else {
        audio.interrupt();
      }
      for (let attempt = 0; !frames.some(frame => frame.kind === 'speech_interrupted') && attempt < 100; attempt++) {
        await new Promise(resolve => setTimeout(resolve, 2));
      }
      const elapsed = performance.now() - began;
      assert.ok(elapsed < 250, `local interrupt took ${elapsed} ms`);
      assert.equal(audio.speechStatus('active-answer')?.status, 'interrupted');
      assert.equal(frames.filter(frame => frame.kind === 'speech_interrupted').length, 1);
      await Promise.race([ttsClosed, new Promise<never>((_, reject) => setTimeout(
        () => reject(new Error('TTS socket survived interruption')), 250))]);
      assert.ok(ttsPeer);
      assert.equal(frames.filter(frame => frame.kind === 'transcript').length, 0);
    } finally {
      audio.stop();
      for (const client of server.clients) client.terminate();
      await new Promise<void>(resolve => server.close(() => resolve()));
    }
  });
}
