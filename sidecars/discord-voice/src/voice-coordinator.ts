import {
  joinVoiceChannel,
  type CreateVoiceConnectionOptions,
  type JoinVoiceChannelOptions,
} from '@discordjs/voice';
import { GatewayBridge } from './gateway-bridge.js';
import type { Frame, StartFrame } from './protocol.js';

type VoiceHandle = { destroy(): void };
type Join = (options: CreateVoiceConnectionOptions & JoinVoiceChannelOptions) => VoiceHandle;
type GatewaySend = (binding: StartFrame, payload: unknown) => boolean;
type Active = { binding: StartFrame; bridge: GatewayBridge; connection: VoiceHandle };

/** The process owns only audio connections. Rust owns the bot gateway and authorization. */
export class VoiceCoordinator {
  private readonly activeByGuild = new Map<string, Active>();
  private readonly lastGeneration = new Map<string, number>();

  constructor(
    private readonly join: Join = joinVoiceChannel,
    private readonly sendGateway: GatewaySend = () => false,
  ) {}

  start(binding: StartFrame): void {
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
    this.activeByGuild.set(binding.guildId, { binding, bridge, connection });
    this.lastGeneration.set(binding.guildId, binding.generation);
  }

  stop(conversationId: string, generation: number): boolean {
    const active = [...this.activeByGuild.values()].find(
      item => item.binding.conversationId === conversationId &&
        item.binding.generation === generation,
    );
    if (!active) return false;
    this.activeByGuild.delete(active.binding.guildId);
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

  shutdown(): void {
    for (const active of [...this.activeByGuild.values()]) {
      this.stop(active.binding.conversationId, active.binding.generation);
    }
  }
}
