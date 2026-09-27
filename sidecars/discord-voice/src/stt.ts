import WebSocket, { type RawData } from 'ws';
import { parseSttEvent, type SttEvent, type SttProvider } from './stt-wire.js';

const MAX_PCM_CHUNK_BYTES = 65_536;
const MAX_BUFFERED_BYTES = 1_048_576;

export type SttSession = { writePcm(chunk: Buffer): void; close(): void };
export type SttOptions = {
  provider: SttProvider;
  apiKey: string;
  onEvent(event: SttEvent): void;
  onError(error: Error): void;
  signal?: AbortSignal;
  /** Local contract servers supply an endpoint; production uses vendor URLs. */
  endpoint?: string;
};

function providerUrl(provider: SttProvider, endpoint?: string): string {
  const base = endpoint ?? (provider === 'deepgram' ? 'wss://api.deepgram.com' : 'wss://api.elevenlabs.io');
  const url = new URL(provider === 'deepgram' ? '/v2/listen' : '/v1/speech-to-text/realtime', base);
  if (provider === 'deepgram') {
    url.searchParams.set('model', 'flux-general-en');
    url.searchParams.set('encoding', 'linear16');
    url.searchParams.set('sample_rate', '16000');
  } else {
    url.searchParams.set('model_id', 'scribe_v2_realtime');
    url.searchParams.set('audio_format', 'pcm_16000');
    url.searchParams.set('commit_strategy', 'vad');
  }
  return url.toString();
}

export async function openSttSession(options: SttOptions): Promise<SttSession> {
  if (!options.apiKey.trim()) throw new Error(`${options.provider} STT key is missing`);
  const headers = options.provider === 'deepgram'
    ? { Authorization: `Token ${options.apiKey}` }
    : { 'xi-api-key': options.apiKey };
  const socket = new WebSocket(providerUrl(options.provider, options.endpoint), {
    headers, handshakeTimeout: 5_000, maxPayload: 131_072, perMessageDeflate: false,
  });
  let closed = false;
  let rejectHandshake: ((error: Error) => void) | undefined;
  const abort = (): void => {
    if (closed) return;
    closed = true;
    socket.terminate();
    rejectHandshake?.(new Error('STT connection cancelled'));
  };
  // A provider may emit its first turn frame in the same event-loop tick as
  // the handshake. Register before awaiting open so that frame is not lost.
  socket.on('message', (data: RawData, binary: boolean) => {
    if (closed || binary) return;
    const event = parseSttEvent(options.provider, data.toString());
    if (event) options.onEvent(event);
  });
  try {
    await new Promise<void>((resolve, reject) => {
      rejectHandshake = reject;
      const onOpen = (): void => {
        socket.off('error', onHandshakeError);
        socket.off('unexpected-response', onUnexpectedResponse);
        resolve();
      };
      const onUnexpectedResponse = (_request: unknown, response: { statusCode?: number }): void => {
        closed = true;
        socket.terminate();
        reject(new Error(`${options.provider} STT handshake failed (HTTP ${response.statusCode})`));
      };
      const onHandshakeError = (): void => {
        closed = true;
        reject(new Error(`${options.provider} STT connection failed`));
      };
      socket.once('open', onOpen);
      socket.once('unexpected-response', onUnexpectedResponse);
      socket.once('error', onHandshakeError);
      options.signal?.addEventListener('abort', abort, { once: true });
      if (options.signal?.aborted) abort();
    });
  } catch (error) {
    options.signal?.removeEventListener('abort', abort);
    socket.terminate();
    throw error;
  } finally {
    rejectHandshake = undefined;
  }
  if (closed) throw new Error('STT connection cancelled');
  socket.on('error', () => {
    if (!closed) options.onError(new Error(`${options.provider} STT stream failed`));
  });
  socket.on('close', () => {
    if (!closed) options.onError(new Error(`${options.provider} STT stream disconnected`));
    closed = true;
    options.signal?.removeEventListener('abort', abort);
  });
  return {
    writePcm(chunk: Buffer): void {
      if (closed || socket.readyState !== WebSocket.OPEN) throw new Error('STT stream is closed');
      if (chunk.length === 0 || chunk.length > MAX_PCM_CHUNK_BYTES || chunk.length % 2 !== 0) {
        throw new Error('Invalid 16-bit PCM audio chunk');
      }
      const frame = options.provider === 'deepgram' ? chunk : JSON.stringify({
        message_type: 'input_audio_chunk', audio_base_64: chunk.toString('base64'),
      });
      const bytes = typeof frame === 'string' ? Buffer.byteLength(frame) : frame.length;
      if (socket.bufferedAmount + bytes > MAX_BUFFERED_BYTES) throw new Error('STT send queue is full');
      socket.send(frame);
    },
    close(): void {
      if (closed) return;
      closed = true;
      options.signal?.removeEventListener('abort', abort);
      if (socket.readyState === WebSocket.OPEN) socket.close(1000);
      else socket.terminate();
    },
  };
}
