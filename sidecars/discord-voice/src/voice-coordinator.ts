import {
  joinVoiceChannel,
  type CreateVoiceConnectionOptions,
  type JoinVoiceChannelOptions,
  type VoiceConnection,
} from '@discordjs/voice';
import { GatewayBridge } from './gateway-bridge.js';
import type { Frame, StartFrame } from './protocol.js';
import { VoiceAudio, type SpeechConfig } from './speech-runtime.js';
import type { SpeechReceipt } from './speech-queue.js';

type VoiceHandle = { destroy(): void };
type Join = (options: CreateVoiceConnectionOptions & JoinVoiceChannelOptions) => VoiceHandle;
type GatewaySend = (binding: StartFrame, payload: unknown) => boolean;
type Active = { binding: StartFrame; bridge: GatewayBridge; connection: VoiceHandle; audio?: VoiceAudio };

/** The process owns only audio connections. Rust owns the bot gateway and authorization. */
export class VoiceCoordinator {
  private readonly activeByGuild = new Map<string, Active>();
  private readonly lastGeneration = new Map<string, number>();

  constructor(
    private readonly join: Join = joinVoiceChannel,
    private readonly sendGateway: GatewaySend = () => false,
    private readonly emitAudio: (frame: unknown) => boolean = () => false,
  ) {}

  start(binding: StartFrame, speech?: SpeechConfig): void {
    const current = this.activeByGuild.get(binding.guildId);
    if (current) {
      if (current.binding.conversationId === binding.conversationId &&
          current.binding.generation === binding.generation &&
          current.binding.channelId === binding.channelId) return;
      throw new Error(`Guild voice is already bound to ${current.binding.conversationId}`);
    }
    if (binding.generation <= (this.lastGeneration.get(binding.guildId) ?? 0)) {
      throw new Error('Stale voice binding generation');
    }
    const bridge = new GatewayBridge(
      binding.guildId, binding.conversationId, binding.botUserId,
      binding.generation, payload => this.sendGateway(binding, payload),
    );
    const connection = this.join({
      guildId: binding.guildId,
      channelId: binding.channelId,
      selfDeaf: false,
      selfMute: false,
      adapterCreator: methods => bridge.create(methods),
    });
    let audio: VoiceAudio | undefined;
    try {
      if (speech) audio = new VoiceAudio(connection as VoiceConnection, binding, speech, frame => {
        const sent = this.emitAudio(frame);
        if (frame && typeof frame === 'object' && (frame as { kind?: string }).kind === 'audio_failure') {
          queueMicrotask(() => { this.stop(binding.conversationId, binding.generation); });
        }
        return sent;
      });
    } catch (error) {
      connection.destroy();
      bridge.destroy();
      throw error;
    }
    this.activeByGuild.set(binding.guildId, { binding, bridge, connection, audio });
    this.lastGeneration.set(binding.guildId, binding.generation);
    if (audio) void audio.start();
  }

  stop(conversationId: string, generation: number): boolean {
    const active = [...this.activeByGuild.values()].find(
      item => item.binding.conversationId === conversationId &&
        item.binding.generation === generation,
    );
    if (!active) return false;
    this.activeByGuild.delete(active.binding.guildId);
    active.audio?.stop();
    active.connection.destroy();
    active.bridge.destroy();
    return true;
  }

  onFrame(frame: Frame): void {
    if (frame.kind !== 'voice_state' && frame.kind !== 'voice_server') return;
    const active = this.activeByGuild.get(frame.guildId);
    if (!active || frame.conversationId !== active.binding.conversationId ||
        frame.generation !== active.binding.generation) return;
    if (frame.kind === 'voice_state' && frame.userId === active.binding.ownerId &&
        frame.channelId !== active.binding.channelId) {
      this.stop(active.binding.conversationId, active.binding.generation);
      return;
    }
    active.bridge.onFrame(frame);
  }

  status(conversationId: string): StartFrame | undefined {
    return [...this.activeByGuild.values()]
      .find(item => item.binding.conversationId === conversationId)?.binding;
  }

  audioStatus(conversationId: string): string | undefined {
    return [...this.activeByGuild.values()]
      .find(item => item.binding.conversationId === conversationId)?.audio?.status;
  }

  speak(conversationId: string, generation: number, utteranceId: string, text: string): SpeechReceipt {
    const active = [...this.activeByGuild.values()].find(item =>
      item.binding.conversationId === conversationId && item.binding.generation === generation);
    if (!active?.audio) throw new Error('Voice binding is not listening');
    return active.audio.speak(utteranceId, text);
  }

  interrupt(conversationId: string, generation: number): boolean {
    const active = [...this.activeByGuild.values()].find(item =>
      item.binding.conversationId === conversationId && item.binding.generation === generation);
    if (!active?.audio) return false;
    active.audio.interrupt();
    return true;
  }

  speechStatus(conversationId: string, generation: number, utteranceId: string): SpeechReceipt | undefined {
    const active = [...this.activeByGuild.values()].find(item =>
      item.binding.conversationId === conversationId && item.binding.generation === generation);
    return active?.audio?.speechStatus(utteranceId);
  }

  shutdown(): void {
    for (const active of [...this.activeByGuild.values()]) {
      this.stop(active.binding.conversationId, active.binding.generation);
    }
  }
}
