import { chmod, lstat, unlink } from 'node:fs/promises';
import { createConnection, createServer, type Server, type Socket } from 'node:net';
import { parseFrame, type Frame } from './protocol.js';
import { VoiceCoordinator } from './voice-coordinator.js';
import { loadSpeechConfig } from './speech-runtime.js';

const MAX_FRAME_BYTES = 32_768;
const MAX_QUEUED_BYTES = 1_048_576;

/** Reclaim only a dead Unix socket from a crashed previous sidecar. */
async function prepareSocketPath(path: string): Promise<void> {
  const previous = await lstat(path).catch(error => {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw error;
  });
  if (!previous) return;
  if (!previous.isSocket()) throw new Error('Voice IPC path exists and is not a socket');
  if (process.getuid && previous.uid !== process.getuid()) {
    throw new Error('Voice IPC socket belongs to another user');
  }
  const state = await new Promise<'live' | 'stale' | 'gone'>((resolve, reject) => {
    const probe = createConnection(path);
    const timer = setTimeout(() => { probe.destroy(); reject(new Error('Voice IPC socket probe timed out')); }, 500);
    probe.once('connect', () => {
      clearTimeout(timer);
      probe.destroy();
      resolve('live');
    });
    probe.once('error', (error: NodeJS.ErrnoException) => {
      clearTimeout(timer);
      probe.destroy();
      if (error.code === 'ECONNREFUSED') resolve('stale');
      else if (error.code === 'ENOENT') resolve('gone');
      else reject(error);
    });
  });
  if (state === 'live') throw new Error('Voice IPC socket is already active');
  if (state === 'gone') return;
  const current = await lstat(path).catch(error => {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return undefined;
    throw error;
  });
  if (!current) return;
  if (!current.isSocket() || current.dev !== previous.dev || current.ino !== previous.ino) {
    throw new Error('Voice IPC socket changed during recovery');
  }
  await unlink(path);
}

/** One Rust daemon connection over a mode-0600 Unix socket. */
export class VoiceIpcServer {
  private readonly server: Server;
  private readonly coordinator: VoiceCoordinator;
  private readonly managedCoordinator: boolean;
  private client?: Socket;

  constructor(private readonly path: string, coordinator?: VoiceCoordinator) {
    this.managedCoordinator = coordinator === undefined;
    this.coordinator = coordinator ?? new VoiceCoordinator(undefined, (binding, payload) => {
      return this.send({ version: 1, kind: 'gateway_send', conversationId: binding.conversationId,
        generation: binding.generation, guildId: binding.guildId, payload });
    }, frame => this.send(frame));
    this.server = createServer(socket => this.accept(socket));
  }

  async listen(): Promise<void> {
    const previous = process.umask(0o177);
    try {
      await prepareSocketPath(this.path);
      await new Promise<void>((resolve, reject) => {
        this.server.once('error', reject);
        this.server.listen(this.path, () => {
          this.server.off('error', reject);
          resolve();
        });
      });
      await chmod(this.path, 0o600);
    } finally {
      process.umask(previous);
    }
  }

  async close(): Promise<void> {
    this.client?.destroy();
    this.coordinator.shutdown();
    await new Promise<void>(resolve => this.server.close(() => resolve()));
    await unlink(this.path).catch(error => {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error;
    });
  }

  private accept(socket: Socket): void {
    if (this.client && !this.client.destroyed) {
      socket.destroy();
      return;
    }
    this.client = socket;
    socket.on('error', () => { /* close handler tears down the binding */ });
    let pending = Buffer.alloc(0);
    socket.on('data', (chunk: Buffer) => {
      pending = Buffer.concat([pending, chunk]);
      if (pending.length > MAX_FRAME_BYTES && pending.indexOf(10) < 0) {
        socket.destroy(new Error('IPC frame too large'));
        return;
      }
      let newline: number;
      while ((newline = pending.indexOf(10)) >= 0) {
        const line = pending.subarray(0, newline);
        pending = pending.subarray(newline + 1);
        if (line.length > MAX_FRAME_BYTES) {
          socket.destroy(new Error('IPC frame too large'));
          return;
        }
        this.dispatch(line.toString('utf8'));
      }
    });
    socket.on('close', () => {
      if (this.client === socket) {
        this.client = undefined;
        this.coordinator.shutdown();
      }
    });
  }

  private dispatch(raw: string): void {
    let frame: Frame;
    try {
      frame = parseFrame(raw);
      switch (frame.kind) {
        case 'start':
          this.coordinator.start(frame, this.managedCoordinator ? loadSpeechConfig(process.env) : undefined);
          break;
        case 'stop':
          if (!this.coordinator.stop(frame.conversationId, frame.generation)) {
            throw new Error('Voice binding not active');
          }
          break;
        case 'interrupt':
          if (!this.coordinator.interrupt(frame.conversationId, frame.generation)) {
            throw new Error('Voice binding is not listening');
          }
          break;
        case 'status':
          this.send({ version: 1, kind: 'reply', requestId: frame.requestId, ok: true,
            binding: this.coordinator.status(frame.conversationId) ?? null,
            state: this.coordinator.audioStatus(frame.conversationId) ?? null });
          return;
        case 'voice_state':
        case 'voice_server':
          this.coordinator.onFrame(frame);
          return;
        case 'speak':
          this.send({ version: 1, kind: 'reply', requestId: frame.requestId, ok: true,
            receipt: this.coordinator.speak(frame.conversationId, frame.generation,
              frame.utteranceId, frame.text) });
          return;
        case 'speech_status':
          this.send({ version: 1, kind: 'reply', requestId: frame.requestId, ok: true,
            receipt: this.coordinator.speechStatus(frame.conversationId, frame.generation,
              frame.utteranceId) ?? null });
          return;
      }
      this.send({ version: 1, kind: 'reply', requestId: frame.requestId, ok: true });
    } catch (error) {
      let requestId: unknown = null;
      try { requestId = (JSON.parse(raw) as Record<string, unknown>).requestId ?? null; } catch { /* malformed JSON */ }
      this.send({ version: 1, kind: 'reply', requestId, ok: false,
        error: error instanceof Error ? error.message : 'IPC request failed' });
    }
  }

  private send(value: unknown): boolean {
    if (!this.client || this.client.destroyed) return false;
    const frame = `${JSON.stringify(value)}\n`;
    if (this.client.writableLength + Buffer.byteLength(frame) > MAX_QUEUED_BYTES) return false;
    try {
      // `write()` returning false means the frame was accepted into Node's
      // buffer. The gateway adapter must not treat that as a failed send.
      this.client.write(frame);
      return true;
    } catch {
      return false;
    }
  }
}
