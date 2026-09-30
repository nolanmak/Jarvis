import WebSocket, { type RawData } from 'ws';
import type { SttProvider } from './stt-wire.js';
import { ProviderError } from './provider-error.js';

const PROVIDER_DEADLINE_MS = 30_000;
const MAX_QUEUED_AUDIO_BYTES = 1_048_576;

export type TtsOptions = {
  provider: SttProvider;
  apiKey: string;
  voiceId?: string;
  text: string;
  signal?: AbortSignal;
  /** Local protocol tests supply an endpoint; production uses vendor URLs. */
  endpoint?: string;
};

class AudioQueue {
  private chunks: Buffer[] = [];
  private queuedBytes = 0;
  private done = false;
  private error?: Error;
  private wake?: () => void;

  push(chunk: Buffer): void {
    if (this.done) return;
    if (this.queuedBytes + chunk.length > MAX_QUEUED_AUDIO_BYTES) {
      this.fail(new Error('TTS audio queue is full'));
      return;
    }
    this.chunks.push(chunk);
    this.queuedBytes += chunk.length;
    this.wake?.();
  }

  finish(): void { this.done = true; this.wake?.(); }
  fail(error: Error): void {
    if (this.done) return;
    this.error = error;
    this.done = true;
    this.wake?.();
  }

  async next(): Promise<Buffer | null> {
    while (this.chunks.length === 0 && !this.done) {
      await new Promise<void>(resolve => { this.wake = resolve; });
      this.wake = undefined;
    }
    if (this.error) throw this.error;
    const chunk = this.chunks.shift();
    if (!chunk) return null;
    this.queuedBytes -= chunk.length;
    return chunk;
  }
}

function deepgramTextChunks(text: string): string[] {
  const chunks: string[] = [];
  for (let offset = 0; offset < text.length; offset += 1800) {
    chunks.push(text.slice(offset, offset + 1800));
  }
  return chunks;
}

async function* deepgramTts(options: TtsOptions, signal: AbortSignal): AsyncGenerator<Buffer> {
  const url = new URL('/v1/speak', options.endpoint ?? 'wss://api.deepgram.com');
  url.searchParams.set('model', 'aura-2-thalia-en');
  url.searchParams.set('encoding', 'linear16');
  url.searchParams.set('sample_rate', '24000');
  url.searchParams.set('container', 'none');
  const socket = new WebSocket(url, {
    headers: { Authorization: `Token ${options.apiKey}` },
    handshakeTimeout: 5_000, maxPayload: 1_048_576, perMessageDeflate: false,
  });
  const queue = new AudioQueue();
  const abort = (): void => { queue.fail(new Error('TTS cancelled')); socket.terminate(); };
  signal.addEventListener('abort', abort, { once: true });
  try {
    await new Promise<void>((resolve, reject) => {
      const onOpen = (): void => {
        socket.off('error', onError);
        socket.off('unexpected-response', onUnexpected);
        resolve();
      };
      const onError = (): void => reject(new Error('Deepgram TTS connection failed'));
      const onUnexpected = (_request: unknown, response: { statusCode?: number }): void => {
        socket.terminate();
        reject(new ProviderError('deepgram', 'TTS', String(response.statusCode),
          `Deepgram TTS handshake failed (HTTP ${response.statusCode})`));
      };
      socket.once('open', onOpen);
      socket.once('error', onError);
      socket.once('unexpected-response', onUnexpected);
    });
    socket.on('message', (data: RawData, binary: boolean) => {
      if (binary) {
        queue.push(Buffer.from(data as Buffer));
        return;
      }
      let frame: unknown;
      try { frame = JSON.parse(data.toString()) as unknown; } catch { return; }
      if (!frame || typeof frame !== 'object') return;
      const message = frame as Record<string, unknown>;
      if (message.type === 'Flushed') queue.finish();
      else if (message.type === 'Error') {
        const code = typeof message.code === 'string' || typeof message.code === 'number'
          ? String(message.code) : 'provider_error';
        queue.fail(new ProviderError('deepgram', 'TTS', code, `Deepgram TTS provider error (${code})`));
      }
    });
    socket.on('error', () => queue.fail(new Error('Deepgram TTS stream failed')));
    socket.on('close', () => queue.fail(new Error('Deepgram TTS stream closed before flush')));
    for (const text of deepgramTextChunks(options.text)) {
      socket.send(JSON.stringify({ type: 'Speak', text }));
    }
    socket.send(JSON.stringify({ type: 'Flush' }));
    while (true) {
      const chunk = await queue.next();
      if (!chunk) break;
      yield chunk;
    }
  } finally {
    signal.removeEventListener('abort', abort);
    if (socket.readyState === WebSocket.OPEN) socket.close(1000);
    else if (socket.readyState !== WebSocket.CLOSED) socket.terminate();
  }
}

async function* elevenLabsTts(options: TtsOptions, signal: AbortSignal): AsyncGenerator<Buffer> {
  if (!options.voiceId) throw new Error('ElevenLabs TTS voice ID is missing');
  const url = new URL(`/v1/text-to-speech/${encodeURIComponent(options.voiceId)}/stream`,
    options.endpoint ?? 'https://api.elevenlabs.io');
  url.searchParams.set('output_format', 'pcm_24000');
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'xi-api-key': options.apiKey, 'Content-Type': 'application/json' },
    body: JSON.stringify({ text: options.text, model_id: 'eleven_flash_v2_5' }),
    signal,
  });
  if (!response.ok) throw new ProviderError('elevenlabs', 'TTS', String(response.status),
    `ElevenLabs TTS request failed (HTTP ${response.status})`);
  if (!response.body) throw new Error('ElevenLabs TTS returned no audio stream');
  const reader = response.body.getReader();
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value.length) yield Buffer.from(value);
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}

/** Raw mono 24 kHz signed 16-bit PCM, yielded before synthesis completes. */
export async function* streamTts(options: TtsOptions): AsyncGenerator<Buffer> {
  if (!options.apiKey.trim()) throw new Error(`${options.provider} TTS key is missing`);
  if (!options.text.trim() || options.text.length > 12_000) throw new Error('Invalid TTS text');
  const signal = options.signal
    ? AbortSignal.any([options.signal, AbortSignal.timeout(PROVIDER_DEADLINE_MS)])
    : AbortSignal.timeout(PROVIDER_DEADLINE_MS);
  if (options.provider === 'deepgram') yield* deepgramTts(options, signal);
  else yield* elevenLabsTts(options, signal);
}

/** Retry the same utterance only when no provider audio has escaped to playback. */
export async function* streamTtsWithFallback(
  primary: TtsOptions, alternate: TtsOptions | undefined, onSwitch: () => void,
): AsyncGenerator<Buffer> {
  let emitted = false;
  try {
    for await (const chunk of streamTts(primary)) {
      emitted = true;
      yield chunk;
    }
    return;
  } catch (error) {
    if (emitted || primary.signal?.aborted || !(error instanceof ProviderError) ||
        !error.exhausted || !alternate) throw error;
    onSwitch();
    yield* streamTts(alternate);
  }
}
