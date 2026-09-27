import assert from 'node:assert/strict';
import test from 'node:test';
import { WebSocketServer } from 'ws';
import { openSttSession } from '../src/stt.js';
import type { SttEvent } from '../src/stt-wire.js';

for (const provider of ['deepgram', 'elevenlabs'] as const) {
  test(`${provider} streams PCM to its provider protocol and emits only committed text`, async () => {
    const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
    await new Promise<void>(resolve => server.once('listening', resolve));
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('expected TCP address');
    const seen: { url?: string; authorization?: string; apiKey?: string; frame?: unknown; binary?: boolean } = {};
    const received = new Promise<void>(resolve => {
      server.once('connection', (socket, request) => {
        seen.url = request.url;
        seen.authorization = request.headers.authorization;
        seen.apiKey = request.headers['xi-api-key'] as string | undefined;
        socket.once('message', (data, binary) => {
          seen.binary = binary;
          seen.frame = binary ? Buffer.from(data as Buffer) : JSON.parse(data.toString()) as unknown;
          if (provider === 'deepgram') {
            socket.send(JSON.stringify({ type: 'TurnInfo', event: 'Update', turn_index: 0, transcript: 'hel' }));
            socket.send(JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn', turn_index: 0, transcript: 'hello' }));
          } else {
            socket.send(JSON.stringify({ message_type: 'partial_transcript', text: 'hel' }));
            socket.send(JSON.stringify({ message_type: 'committed_transcript', text: 'hello' }));
          }
          resolve();
        });
      });
    });
    const events: SttEvent[] = [];
    const errors: Error[] = [];
    const endpoint = `ws://127.0.0.1:${address.port}`;
    const session = await openSttSession({ provider, apiKey: 'synthetic-test-key', endpoint,
      onEvent: event => events.push(event), onError: error => errors.push(error) });
    try {
      session.writePcm(Buffer.from([1, 0, 2, 0]));
      await received;
      for (let attempt = 0; events.length < 2 && attempt < 100; attempt++) {
        await new Promise(resolve => setTimeout(resolve, 5));
      }
      assert.deepEqual(events.map(event => event.kind), ['partial', 'final']);
      assert.equal(errors.length, 0);
      if (provider === 'deepgram') {
        assert.match(seen.url ?? '', /\/v2\/listen\?model=flux-general-en&encoding=linear16&sample_rate=16000/);
        assert.equal(seen.authorization, 'Token synthetic-test-key');
        assert.equal(seen.binary, true);
        assert.deepEqual(seen.frame, Buffer.from([1, 0, 2, 0]));
      } else {
        assert.match(seen.url ?? '', /\/v1\/speech-to-text\/realtime\?model_id=scribe_v2_realtime/);
        assert.equal(seen.apiKey, 'synthetic-test-key');
        assert.equal(seen.binary, false);
        assert.deepEqual(seen.frame, { message_type: 'input_audio_chunk', audio_base_64: 'AQACAA==' });
      }
    } finally {
      session.close();
      for (const client of server.clients) client.terminate();
      await new Promise<void>(resolve => server.close(() => resolve()));
    }
  });
}
