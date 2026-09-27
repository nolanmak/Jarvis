import assert from 'node:assert/strict';
import { mkdtemp, stat, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createConnection, type Socket } from 'node:net';
import test from 'node:test';
import { VoiceIpcServer } from '../src/ipc.js';
import { VoiceCoordinator } from '../src/voice-coordinator.js';

function readLine(socket: Socket): Promise<unknown> {
  return new Promise((resolve, reject) => {
    let data = '';
    socket.on('data', function onData(chunk: Buffer) {
      data += chunk.toString();
      const newline = data.indexOf('\n');
      if (newline >= 0) {
        socket.off('data', onData);
        resolve(JSON.parse(data.slice(0, newline)) as unknown);
      }
    });
    socket.once('error', reject);
  });
}

test('Unix socket is private and dispatches start/stop with versioned replies', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'jarvis-voice-ipc-'));
  const path = join(directory, 'voice.sock');
  let destroyed = 0;
  const coordinator = new VoiceCoordinator(() => ({ destroy() { destroyed++; } }), () => true);
  const service = new VoiceIpcServer(path, coordinator);
  await service.listen();
  const socket = createConnection(path);
  try {
    assert.equal((await stat(path)).mode & 0o777, 0o600);
    socket.write(JSON.stringify({ version: 1, kind: 'start', requestId: 'req-1', guildId: 'guild-1',
      channelId: 'voice-1', conversationId: 'text-1', ownerId: 'owner-1', botUserId: 'bot-1', generation: 1 }) + '\n');
    assert.deepEqual(await readLine(socket), { version: 1, kind: 'reply', requestId: 'req-1', ok: true });
    socket.write(JSON.stringify({ version: 1, kind: 'stop', requestId: 'req-2',
      conversationId: 'text-1', generation: 1 }) + '\n');
    assert.deepEqual(await readLine(socket), { version: 1, kind: 'reply', requestId: 'req-2', ok: true });
    assert.equal(destroyed, 1);
  } finally {
    socket.destroy();
    await service.close();
    await rm(directory, { recursive: true, force: true });
  }
});
