import { lstat } from 'node:fs/promises';
import { createConnection } from 'node:net';
import { isAbsolute, join } from 'node:path';
import { loadSpeechConfig } from './speech-runtime.js';

export type VoicePreflight = {
  node: string;
  socket: string;
  sttProvider: string;
  ttsProvider: string;
};

/** Read-only checks for the installed sidecar. Never return credential values. */
export async function checkVoicePreflight(env: NodeJS.ProcessEnv): Promise<VoicePreflight> {
  const version = process.versions.node.split('.').map(Number);
  if ((version[0] ?? 0) !== 24 || (version[1] ?? 0) < 17) {
    throw new Error('Discord voice requires pinned Node 24.17.0 or newer within major 24');
  }
  const config = loadSpeechConfig(env);
  const socket = env.AUGMENTAGENT_DISCORD_VOICE_SOCKET ??
    (env.XDG_RUNTIME_DIR ? join(env.XDG_RUNTIME_DIR, 'augmentagent', 'discord-voice.sock') : undefined);
  if (!socket || !isAbsolute(socket)) {
    throw new Error('Set an absolute AUGMENTAGENT_DISCORD_VOICE_SOCKET or XDG_RUNTIME_DIR');
  }
  const info = await lstat(socket);
  if (!info.isSocket()) throw new Error('Discord voice IPC path is not a Unix socket');
  if (process.getuid && info.uid !== process.getuid()) {
    throw new Error('Discord voice IPC socket belongs to another user');
  }
  if ((info.mode & 0o777) !== 0o600) {
    throw new Error('Discord voice IPC socket must have mode 0600');
  }
  await new Promise<void>((resolve, reject) => {
    const probe = createConnection(socket);
    const timer = setTimeout(() => {
      probe.destroy();
      reject(new Error('Discord voice IPC listener did not respond'));
    }, 500);
    probe.once('connect', () => {
      clearTimeout(timer);
      probe.destroy();
      resolve();
    });
    probe.once('error', () => {
      clearTimeout(timer);
      probe.destroy();
      reject(new Error('Discord voice IPC socket has no active listener'));
    });
  });
  return { node: process.versions.node, socket,
    sttProvider: config.sttProvider, ttsProvider: config.ttsProvider };
}
