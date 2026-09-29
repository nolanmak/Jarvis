import type {
  DiscordGatewayAdapterImplementerMethods,
  DiscordGatewayAdapterLibraryMethods,
} from '@discordjs/voice';
import type { Frame, GatewayServerFrame, GatewayStateFrame } from './protocol.js';

type GatewaySend = (payload: {
  op: 4;
  d: { guild_id: string; channel_id: string | null; self_deaf: boolean; self_mute: boolean };
}) => boolean;

/** Connects the Discord voice library to the Rust-owned bot gateway. */
export class GatewayBridge {
  private methods?: DiscordGatewayAdapterLibraryMethods;
  private closed = false;

  constructor(
    private readonly guildId: string,
    private readonly conversationId: string,
    private readonly botUserId: string,
    private readonly generation: number,
    private readonly send: GatewaySend,
  ) {}

  create(methods: DiscordGatewayAdapterLibraryMethods): DiscordGatewayAdapterImplementerMethods {
    if (this.closed || this.methods) throw new Error('Gateway adapter is already in use');
    this.methods = methods;
    return {
      sendPayload: (payload: unknown) => {
        if (this.closed || !this.isVoiceStateSend(payload)) return false;
        return this.send(payload);
      },
      destroy: () => {
        this.closed = true;
        this.methods = undefined;
      },
    };
  }

  onFrame(frame: Frame): void {
    if (this.closed || !this.methods || frame.conversationId !== this.conversationId ||
        frame.generation !== this.generation) return;
    if (frame.kind === 'voice_state' && frame.guildId === this.guildId && frame.userId === this.botUserId) {
      this.methods.onVoiceStateUpdate(this.statePayload(frame));
    } else if (frame.kind === 'voice_server' && frame.guildId === this.guildId) {
      this.methods.onVoiceServerUpdate(this.serverPayload(frame));
    }
  }

  destroy(): void {
    if (this.closed) return;
    this.closed = true;
    const methods = this.methods;
    this.methods = undefined;
    methods?.destroy();
  }

  private isVoiceStateSend(payload: unknown): payload is Parameters<GatewaySend>[0] {
    if (!payload || typeof payload !== 'object') return false;
    const outer = payload as Record<string, unknown>;
    if (outer.op !== 4 || !outer.d || typeof outer.d !== 'object') return false;
    const data = outer.d as Record<string, unknown>;
    return data.guild_id === this.guildId &&
      (data.channel_id === null || typeof data.channel_id === 'string') &&
      typeof data.self_deaf === 'boolean' && typeof data.self_mute === 'boolean';
  }

  private statePayload(frame: GatewayStateFrame): Parameters<DiscordGatewayAdapterLibraryMethods['onVoiceStateUpdate']>[0] {
    return {
      guild_id: frame.guildId,
      user_id: frame.userId,
      channel_id: frame.channelId,
      session_id: frame.sessionId,
    } as Parameters<DiscordGatewayAdapterLibraryMethods['onVoiceStateUpdate']>[0];
  }

  private serverPayload(frame: GatewayServerFrame): Parameters<DiscordGatewayAdapterLibraryMethods['onVoiceServerUpdate']>[0] {
    return {
      guild_id: frame.guildId,
      endpoint: frame.endpoint,
      token: frame.token,
    } as Parameters<DiscordGatewayAdapterLibraryMethods['onVoiceServerUpdate']>[0];
  }
}
