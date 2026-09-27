import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { chmod, mkdtemp, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { checkVoicePreflight } from '../src/doctor.js';

test('doctor checks a private live socket and selected providers without returning keys', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'jarvis-voice-doctor-'));
  const socket = join(directory, 'voice.sock');
  const server = createServer();
  await new Promise<void>(resolve => server.listen(socket, resolve));
  await chmod(socket, 0o600);
  const env = { AUGMENTAGENT_DISCORD_VOICE_SOCKET: socket,
    AUGMENTAGENT_DISCORD_STT_PROVIDER: 'deepgram',
    AUGMENTAGENT_DISCORD_TTS_PROVIDER: 'elevenlabs',
    DEEPGRAM_API_KEY: 'fake-deepgram-secret',
    ELEVENLABS_API_KEY: 'fake-elevenlabs-secret',
    ELEVENLABS_VOICE_ID: 'fake-voice-id' };
  try {
    const result = await checkVoicePreflight(env);
    assert.equal(result.socket, socket);
    assert.equal(result.sttProvider, 'deepgram');
    assert.equal(result.ttsProvider, 'elevenlabs');
    assert.doesNotMatch(JSON.stringify(result), /fake-|secret/);
    const output: Buffer[] = [];
    const child = spawn(process.execPath,
      [new URL('../scripts/doctor.js', import.meta.url).pathname],
      { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    child.stdout.on('data', (chunk: Buffer) => output.push(chunk));
    child.stderr.on('data', (chunk: Buffer) => output.push(chunk));
    const exitCode = await new Promise<number | null>(resolve => child.on('exit', resolve));
    assert.equal(exitCode, 0);
    assert.match(Buffer.concat(output).toString(), /ready-for-live-test/);
    assert.doesNotMatch(Buffer.concat(output).toString(), /fake-|secret/);
    await chmod(socket, 0o666);
    await assert.rejects(checkVoicePreflight(env), /mode 0600/);
    await assert.rejects(checkVoicePreflight({ ...env, ELEVENLABS_API_KEY: '' }), /TTS key is missing/);
  } finally {
    await new Promise<void>(resolve => server.close(() => resolve()));
    if (await stat(socket).then(() => true).catch(() => false)) {
      await chmod(socket, 0o600);
      await assert.rejects(checkVoicePreflight(env), /no active listener/);
    }
    await rm(directory, { recursive: true, force: true });
  }
});
