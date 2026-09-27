import assert from 'node:assert/strict';
import { mkdtemp, stat, rm, writeFile, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createConnection, type Socket } from 'node:net';
import { spawn } from 'node:child_process';
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

test('oversized IPC input is closed without killing the sidecar', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'jarvis-voice-ipc-'));
  const path = join(directory, 'voice.sock');
  const service = new VoiceIpcServer(path, new VoiceCoordinator(() => ({ destroy() {} }), () => true));
  await service.listen();
  const oversized = createConnection(path);
  try {
    await new Promise<void>(resolve => oversized.once('connect', resolve));
    oversized.write('x'.repeat(33_000));
    await new Promise<void>(resolve => oversized.once('close', resolve));
    const healthy = createConnection(path);
    await new Promise<void>(resolve => healthy.once('connect', resolve));
    healthy.write(JSON.stringify({ version: 1, kind: 'status', requestId: 'req-after-error',
      conversationId: 'text-1', generation: 1 }) + '\n');
    assert.deepEqual(await readLine(healthy), { version: 1, kind: 'reply',
      requestId: 'req-after-error', ok: true, binding: null, state: null });
    healthy.destroy();
  } finally {
    oversized.destroy();
    await service.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test('restart recovers a stale socket but never replaces a live listener or a regular file', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'jarvis-voice-restart-'));
  const path = join(directory, 'voice.sock');
  const child = spawn(process.execPath, ['-e',
    'const s=require("node:net").createServer();s.listen(process.argv[1],()=>process.stdout.write("READY\\n"))', path],
    { stdio: ['ignore', 'pipe', 'pipe'] });
  try {
    await new Promise<void>((resolve, reject) => {
      child.stdout!.once('data', chunk => chunk.toString().includes('READY') ? resolve() : reject(new Error('child not ready')));
      child.once('error', reject);
    });
    child.kill('SIGKILL');
    await new Promise<void>(resolve => child.once('close', () => resolve()));
    assert.equal((await stat(path)).isSocket(), true);
    const recovered = new VoiceIpcServer(path, new VoiceCoordinator(() => ({ destroy() {} }), () => true));
    await recovered.listen();
    const socket = createConnection(path);
    try {
      await new Promise<void>(resolve => socket.once('connect', resolve));
      const rival = new VoiceIpcServer(path, new VoiceCoordinator(() => ({ destroy() {} }), () => true));
      await assert.rejects(rival.listen(), /already|active|in use/i);
      socket.write(JSON.stringify({ version: 1, kind: 'status', requestId: 'still-live',
        conversationId: 'text-1', generation: 1 }) + '\n');
      assert.equal((await readLine(socket) as { requestId: string }).requestId, 'still-live');
    } finally {
      socket.destroy();
      await recovered.close();
    }
    await writeFile(path, 'do not replace');
    const regular = new VoiceIpcServer(path, new VoiceCoordinator(() => ({ destroy() {} }), () => true));
    await assert.rejects(regular.listen(), /socket|regular|exists/i);
    assert.equal(await readFile(path, 'utf8'), 'do not replace');
  } finally {
    if (child.exitCode === null) child.kill('SIGKILL');
    await rm(directory, { recursive: true, force: true });
  }
});
