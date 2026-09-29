import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import { createConnection } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

test('sidecar starts on a private socket and exits cleanly on SIGTERM', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'jarvis-voice-main-'));
  const socketPath = join(directory, 'voice.sock');
  const child = spawn(process.execPath, [new URL('../src/main.js', import.meta.url).pathname], {
    env: { ...process.env, AUGMENTAGENT_DISCORD_VOICE_SOCKET: socketPath },
    stdio: 'ignore',
  });
  try {
    let mode: number | undefined;
    for (let attempt = 0; attempt < 100; attempt++) {
      if (child.exitCode !== null) throw new Error(`sidecar exited with ${child.exitCode}`);
      mode = await stat(socketPath).then(item => item.mode & 0o777).catch(() => undefined);
      if (mode !== undefined) break;
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    assert.equal(mode, 0o600);
    const socket = createConnection(socketPath);
    await once(socket, 'connect');
    socket.destroy();
    child.kill('SIGTERM');
    const [code] = await once(child, 'exit');
    assert.equal(code, 0);
    await assert.rejects(stat(socketPath));
  } finally {
    if (child.exitCode === null) child.kill('SIGKILL');
    await rm(directory, { recursive: true, force: true });
  }
});
