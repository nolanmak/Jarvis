import { chmod, unlink } from 'node:fs/promises';
import { createServer, type Server, type Socket } from 'node:net';
import { parseFrame, type Frame } from './protocol.js';
import { VoiceCoordinator } from './voice-coordinator.js';
import { loadSpeechConfig } from './speech-runtime.js';

const MAX_FRAME_BYTES = 32_768;
const MAX_QUEUED_BYTES = 1_048_576;

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
