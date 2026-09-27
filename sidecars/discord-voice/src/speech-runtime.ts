import { once } from 'node:events';
import { PassThrough } from 'node:stream';
import {
  AudioPlayerStatus, EndBehaviorType, NoSubscriberBehavior, StreamType,
  VoiceConnectionStatus, createAudioPlayer, createAudioResource, entersState,
  type PlayerSubscription, type VoiceConnection,
} from '@discordjs/voice';
import * as prism from 'prism-media';
import { mono24ToStereo48, resampleDiscordPcmForStt, type SttResampler } from './audio-format.js';
import type { StartFrame } from './protocol.js';
import { SpeechQueue, type PlaybackSink, type SpeechReceipt } from './speech-queue.js';
import { openSttSession, type SttSession } from './stt.js';
import type { SttEvent, SttProvider } from './stt-wire.js';
import { streamTts } from './tts.js';

export type SpeechConfig = {
  sttProvider: SttProvider;
  ttsProvider: SttProvider;
  sttKey: string;
  ttsKey: string;
  elevenLabsVoiceId?: string;
};

function provider(value: string | undefined): SttProvider {
  if (value === undefined || value === '') return 'deepgram';
  if (value === 'deepgram' || value === 'elevenlabs') return value;
  throw new Error('Speech provider must be deepgram or elevenlabs');
}

/** Validate both independently selected providers before a voice join. */
export function loadSpeechConfig(env: NodeJS.ProcessEnv): SpeechConfig {
  const sttProvider = provider(env.AUGMENTAGENT_DISCORD_STT_PROVIDER);
  const ttsProvider = provider(env.AUGMENTAGENT_DISCORD_TTS_PROVIDER);
  const selectedKey = (kind: SttProvider): string | undefined =>
    kind === 'deepgram' ? env.DEEPGRAM_API_KEY : env.ELEVENLABS_API_KEY;
  const sttKey = selectedKey(sttProvider);
  const ttsKey = selectedKey(ttsProvider);
  if (!sttKey?.trim()) throw new Error(`${sttProvider === 'deepgram' ? 'Deepgram' : 'ElevenLabs'} STT key is missing`);
  if (!ttsKey?.trim()) throw new Error(`${ttsProvider === 'deepgram' ? 'Deepgram' : 'ElevenLabs'} TTS key is missing`);
  if (ttsProvider === 'elevenlabs' && !env.ELEVENLABS_VOICE_ID?.trim()) {
    throw new Error('ElevenLabs TTS voice ID is missing');
  }
  return { sttProvider, ttsProvider, sttKey, ttsKey,
    elevenLabsVoiceId: env.ELEVENLABS_VOICE_ID };
}

class DiscordPlaybackSink implements PlaybackSink {
  private readonly player = createAudioPlayer({ behaviors: { noSubscriber: NoSubscriberBehavior.Pause } });
  private readonly subscription: PlayerSubscription;
  private current?: { source: PassThrough; converter: ReturnType<typeof mono24ToStereo48> };

  constructor(connection: VoiceConnection) {
    const subscription = connection.subscribe(this.player);
    if (!subscription) throw new Error('Discord voice connection cannot play audio');
    this.subscription = subscription;
  }

  async play(audio: AsyncIterable<Buffer>): Promise<void> {
    const source = new PassThrough({ highWaterMark: 65_536 });
    const converter = mono24ToStereo48();
    source.on('error', () => {});
    converter.on('error', () => {});
    converter.pipe(source);
    this.current = { source, converter };
    this.player.play(createAudioResource(source, { inputType: StreamType.Raw }));
    let received = false;
    try {
      for await (const chunk of audio) {
        if (this.current?.converter !== converter) throw new Error('Speech playback interrupted');
        if (!chunk.length) continue;
        received = true;
        if (!converter.write(chunk)) {
          await Promise.race([once(converter, 'drain'), once(converter, 'close')]);
        }
      }
      if (!received) throw new Error('TTS provider returned empty audio');
      converter.end();
      await entersState(this.player, AudioPlayerStatus.Idle, 120_000);
    } finally {
      if (this.current?.converter === converter) this.current = undefined;
      converter.destroy();
      source.destroy();
    }
  }

  interrupt(): void {
    this.player.stop(true);
    this.current?.converter.destroy();
    this.current?.source.destroy();
    this.current = undefined;
  }

  stop(): void {
    this.interrupt();
    this.subscription.unsubscribe();
  }
}

/** One owner-only audio receiver and speech output queue per voice binding. */
export class VoiceAudio {
  private readonly sink: DiscordPlaybackSink;
  private readonly speech: SpeechQueue;
  private stt?: SttSession;
  private receiver?: ReturnType<VoiceConnection['receiver']['subscribe']>;
  private decoder?: prism.opus.Decoder;
  private resampler?: SttResampler;
  private cap?: ReturnType<typeof setTimeout>;
  private speechActive = false;
  private finalCounter = 0;
  private readonly committed = new Set<string>();
  private pcmCarry?: number;
  private closed = false;
  private state: 'connecting' | 'listening' | 'failed' | 'stopped' = 'connecting';

  constructor(
    private readonly connection: VoiceConnection,
    private readonly binding: StartFrame,
    private readonly config: SpeechConfig,
    private readonly emit: (frame: unknown) => boolean,
  ) {
    this.sink = new DiscordPlaybackSink(connection);
    this.speech = new SpeechQueue((text, signal) => streamTts({
      provider: config.ttsProvider, apiKey: config.ttsKey,
      voiceId: config.elevenLabsVoiceId, text, signal,
    }), this.sink);
  }

  get status(): string { return this.state; }

  async start(): Promise<void> {
    try {
      await entersState(this.connection, VoiceConnectionStatus.Ready, 20_000);
      if (this.closed) return;
      this.stt = await openSttSession({
        provider: this.config.sttProvider, apiKey: this.config.sttKey,
        onEvent: event => this.onSttEvent(event),
        onError: error => this.fail(error.message),
      });
      if (this.closed) { this.stt.close(); return; }
      const receiver = this.connection.receiver.subscribe(this.binding.ownerId, {
        end: { behavior: EndBehaviorType.Manual },
      });
      const decoder = new prism.opus.Decoder({ rate: 48_000, channels: 2, frameSize: 960 });
      const resampler = resampleDiscordPcmForStt();
      this.receiver = receiver;
      this.decoder = decoder;
      this.resampler = resampler;
      receiver.pipe(decoder).pipe(resampler.input);
      receiver.on('error', () => this.fail('Discord owner audio receive failed'));
      decoder.on('error', () => this.fail('Discord Opus decode failed'));
      resampler.output.on('error', () => this.fail('Discord audio resampling failed'));
      resampler.output.on('data', (chunk: Buffer) => this.forwardPcm(chunk));
      this.state = 'listening';
      this.emitStatus();
    } catch (error) {
      this.fail(error instanceof Error ? error.message : 'Voice audio startup failed');
    }
  }

  speak(utteranceId: string, text: string): SpeechReceipt {
    if (this.closed || this.state !== 'listening') throw new Error('Voice is not listening');
    return this.speech.speak(utteranceId, text);
  }

  speechStatus(utteranceId: string): SpeechReceipt | undefined {
    return this.speech.status(utteranceId);
  }

  interrupt(): void { this.speech.interrupt(); }

  stop(): void {
    if (this.closed) return;
    this.closed = true;
    this.state = 'stopped';
    if (this.cap) clearTimeout(this.cap);
    this.speech.stop();
    this.sink.stop();
    this.receiver?.destroy();
    this.decoder?.destroy();
    this.resampler?.abort();
    this.stt?.close();
    this.emitStatus();
  }

  private forwardPcm(chunk: Buffer): void {
    if (this.closed || !this.stt) return;
    try {
      const pcm = this.pcmCarry === undefined ? chunk : Buffer.concat([Buffer.from([this.pcmCarry]), chunk]);
      const even = pcm.length - pcm.length % 2;
      this.pcmCarry = pcm.length % 2 ? pcm[pcm.length - 1] : undefined;
      for (let offset = 0; offset < even; offset += 2560) {
        this.stt.writePcm(pcm.subarray(offset, Math.min(offset + 2560, even)));
      }
    } catch (error) {
      this.fail(error instanceof Error ? error.message : 'STT audio forwarding failed');
    }
  }

  private onSttEvent(event: SttEvent): void {
    if (this.closed) return;
    if (event.kind === 'speech_start' || (event.kind === 'partial' && !this.speechActive)) {
      this.speechActive = true;
      this.speech.interrupt();
      this.cap = setTimeout(() => this.fail('Owner utterance exceeded 120 seconds'), 120_000);
    }
    if (event.kind !== 'final') return;
    this.speechActive = false;
    if (this.cap) clearTimeout(this.cap);
    const index = event.turnIndex ?? this.finalCounter++;
    const turnId = `voice:${this.binding.generation}:${index}`;
    if (this.committed.has(turnId)) return;
    this.committed.add(turnId);
    this.emit({ version: 1, kind: 'transcript', conversationId: this.binding.conversationId,
      generation: this.binding.generation, guildId: this.binding.guildId,
      ownerId: this.binding.ownerId, turnId, text: event.text });
  }

  private fail(message: string): void {
    if (this.closed) return;
    this.state = 'failed';
    this.emit({ version: 1, kind: 'audio_failure', conversationId: this.binding.conversationId,
      generation: this.binding.generation, guildId: this.binding.guildId, error: message });
    this.stop();
  }

  private emitStatus(): void {
    this.emit({ version: 1, kind: 'audio_status', conversationId: this.binding.conversationId,
      generation: this.binding.generation, guildId: this.binding.guildId, state: this.state });
  }
}
