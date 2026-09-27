import { isAbsolute } from 'node:path';
import { VoiceIpcServer } from './ipc.js';

const socketPath = process.env.AUGMENTAGENT_DISCORD_VOICE_SOCKET;
if (!socketPath || !isAbsolute(socketPath)) {
  throw new Error('AUGMENTAGENT_DISCORD_VOICE_SOCKET must be an absolute path');
}

const service = new VoiceIpcServer(socketPath);
await service.listen();

let stopping = false;
function stop(): void {
  if (stopping) return;
  stopping = true;
  void service.close().catch(error => {
    process.exitCode = 1;
    console.error(error instanceof Error ? error.message : 'Voice sidecar shutdown failed');
  });
}

process.on('SIGTERM', stop);
process.on('SIGINT', stop);
