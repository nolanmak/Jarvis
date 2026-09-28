import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import test from 'node:test';
import { WebSocketServer } from 'ws';
import { streamTts } from '../src/tts.js';

test('Deepgram TTS yields audio before the final websocket chunk', async () => {
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await new Promise<void>(resolve => server.once('listening', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  let releaseLast!: () => void;
  const last = new Promise<void>(resolve => { releaseLast = resolve; });
  const received: unknown[] = [];
  server.once('connection', (socket, request) => {
    assert.match(request.url ?? '', /\/v1\/speak\?model=aura-2-thalia-en/);
    assert.equal(request.headers.authorization, 'Token synthetic-test-key');
    socket.on('message', data => {
      const frame = JSON.parse(data.toString()) as { type: string };
      received.push(frame);
      if (frame.type === 'Flush') {
        socket.send(Buffer.from([1, 0, 2, 0]));
        void last.then(() => {
          socket.send(Buffer.from([3, 0, 4, 0]));
          socket.send(JSON.stringify({ type: 'Flushed' }));
        });
      }
    });
  });
  try {
    const iterator = streamTts({ provider: 'deepgram', apiKey: 'synthetic-test-key', text: 'hello',
      endpoint: `ws://127.0.0.1:${address.port}` })[Symbol.asyncIterator]();
    const first = await iterator.next();
    assert.deepEqual(first.value, Buffer.from([1, 0, 2, 0]));
    assert.equal(first.done, false);
    releaseLast();
    assert.deepEqual((await iterator.next()).value, Buffer.from([3, 0, 4, 0]));
    assert.equal((await iterator.next()).done, true);
    assert.deepEqual(received, [{ type: 'Speak', text: 'hello' }, { type: 'Flush' }]);
  } finally {
    releaseLast();
    for (const client of server.clients) client.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

test('ElevenLabs TTS yields PCM before the HTTP stream finishes', async () => {
  let releaseLast!: () => void;
  const last = new Promise<void>(resolve => { releaseLast = resolve; });
  let seenBody = '';
  const server = createServer(async (request, response) => {
    assert.match(request.url ?? '', /\/v1\/text-to-speech\/voice-test\/stream\?output_format=pcm_24000/);
    assert.equal(request.headers['xi-api-key'], 'synthetic-test-key');
    for await (const chunk of request) seenBody += chunk.toString();
    response.writeHead(200, { 'Content-Type': 'audio/pcm' });
    response.write(Buffer.from([1, 0, 2, 0]));
    await last;
    response.end(Buffer.from([3, 0, 4, 0]));
  });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('expected TCP address');
  try {
    const iterator = streamTts({ provider: 'elevenlabs', apiKey: 'synthetic-test-key',
      voiceId: 'voice-test', text: 'hello', endpoint: `http://127.0.0.1:${address.port}` })[Symbol.asyncIterator]();
    const first = await iterator.next();
    assert.deepEqual(first.value, Buffer.from([1, 0, 2, 0]));
    assert.equal(first.done, false);
    releaseLast();
    assert.deepEqual((await iterator.next()).value, Buffer.from([3, 0, 4, 0]));
    assert.equal((await iterator.next()).done, true);
    assert.deepEqual(JSON.parse(seenBody), { text: 'hello', model_id: 'eleven_flash_v2_5' });
  } finally {
    releaseLast();
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});

for (const status of [401, 403, 429, 500]) {
  test(`Deepgram TTS surfaces HTTP ${status} handshake refusal`, async () => {
    const server = new WebSocketServer({ host: '127.0.0.1', port: 0,
      verifyClient: (_info, callback) => callback(false, status) });
    await new Promise<void>(resolve => server.once('listening', resolve));
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('expected TCP address');
    try {
      await assert.rejects(async () => {
        for await (const _ of streamTts({ provider: 'deepgram', apiKey: 'synthetic-test-key',
          text: 'hello', endpoint: `ws://127.0.0.1:${address.port}` })) { /* no audio expected */ }
      }, new RegExp(`HTTP ${status}`));
    } finally {
      await new Promise<void>(resolve => server.close(() => resolve()));
    }
  });

  test(`ElevenLabs TTS surfaces HTTP ${status} response failure`, async () => {
    const server = createServer((_request, response) => {
      response.writeHead(status);
      response.end();
    });
    await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
    const address = server.address();
    if (!address || typeof address === 'string') throw new Error('expected TCP address');
    try {
      await assert.rejects(async () => {
        for await (const _ of streamTts({ provider: 'elevenlabs', apiKey: 'synthetic-test-key',
          voiceId: 'voice-test', text: 'hello', endpoint: `http://127.0.0.1:${address.port}` })) {
          /* no audio expected */
        }
      }, new RegExp(`HTTP ${status}`));
    } finally {
      await new Promise<void>(resolve => server.close(() => resolve()));
    }
  });
}
