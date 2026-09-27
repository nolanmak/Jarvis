/** Local Unix-socket protocol. Audio and provider keys never travel in frames. */
export type StartFrame = {
  version: 1;
  kind: 'start';
  requestId: string;
  guildId: string;
  channelId: string;
  conversationId: string;
  ownerId: string;
  botUserId: string;
  generation: number;
};

export type SpeakFrame = {
  version: 1;
  kind: 'speak';
  requestId: string;
  conversationId: string;
  generation: number;
  utteranceId: string;
  text: string;
};

export type ControlFrame = {
  version: 1;
  kind: 'stop' | 'interrupt' | 'status';
  requestId: string;
  conversationId: string;
  generation: number;
};

export type GatewayStateFrame = {
  version: 1;
  kind: 'voice_state';
  conversationId: string;
  generation: number;
  guildId: string;
  userId: string;
  channelId: string | null;
  sessionId: string;
};

export type GatewayServerFrame = {
  version: 1;
  kind: 'voice_server';
  conversationId: string;
  generation: number;
  guildId: string;
  endpoint: string;
  token: string;
};

export type Frame = StartFrame | SpeakFrame | ControlFrame | GatewayStateFrame | GatewayServerFrame;

function record(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('IPC frame must be an object');
  }
  return value as Record<string, unknown>;
}

function exact(value: Record<string, unknown>, keys: readonly string[]): void {
  if (Object.keys(value).some(key => !keys.includes(key))) {
    throw new Error('IPC frame contains an unexpected field');
  }
  if (keys.some(key => !(key in value))) {
    throw new Error('IPC frame is missing a field');
  }
}

function id(value: unknown, key: string): string {
  if (typeof value !== 'string' || value.trim() !== value || !value || value.length > 128) {
    throw new Error(`Invalid ${key}`);
  }
  return value;
}

function generation(value: unknown): number {
  if (!Number.isSafeInteger(value) || (value as number) < 1) {
    throw new Error('Invalid binding generation');
  }
  return value as number;
}

export function parseFrame(raw: string): Frame {
  if (Buffer.byteLength(raw) > 32_768) throw new Error('IPC frame too large');
  const value = record(JSON.parse(raw) as unknown);
  if (value.version !== 1) throw new Error('Unsupported IPC version');
  const common = { version: 1 as const, conversationId: id(value.conversationId, 'conversationId'), generation: generation(value.generation) };
  switch (value.kind) {
    case 'start':
      exact(value, ['version', 'kind', 'requestId', 'guildId', 'channelId', 'conversationId', 'ownerId', 'botUserId', 'generation']);
      return { ...common, kind: 'start', requestId: id(value.requestId, 'requestId'),
        guildId: id(value.guildId, 'guildId'), channelId: id(value.channelId, 'channelId'),
        ownerId: id(value.ownerId, 'ownerId'), botUserId: id(value.botUserId, 'botUserId') };
    case 'speak': {
      exact(value, ['version', 'kind', 'requestId', 'conversationId', 'generation', 'utteranceId', 'text']);
      const speech = value.text;
      if (typeof speech !== 'string' || !speech.trim() || speech.length > 12_000) {
        throw new Error('Invalid speech text');
      }
      return { ...common, kind: 'speak', requestId: id(value.requestId, 'requestId'),
        utteranceId: id(value.utteranceId, 'utteranceId'), text: speech };
    }
    case 'stop':
    case 'interrupt':
    case 'status':
      exact(value, ['version', 'kind', 'requestId', 'conversationId', 'generation']);
      return { ...common, kind: value.kind, requestId: id(value.requestId, 'requestId') };
    case 'voice_state':
      exact(value, ['version', 'kind', 'conversationId', 'generation', 'guildId', 'userId', 'channelId', 'sessionId']);
      if (value.channelId !== null) id(value.channelId, 'channelId');
      return { ...common, kind: 'voice_state', guildId: id(value.guildId, 'guildId'),
        userId: id(value.userId, 'userId'), channelId: value.channelId as string | null,
        sessionId: id(value.sessionId, 'sessionId') };
    case 'voice_server':
      exact(value, ['version', 'kind', 'conversationId', 'generation', 'guildId', 'endpoint', 'token']);
      return { ...common, kind: 'voice_server', guildId: id(value.guildId, 'guildId'),
        endpoint: id(value.endpoint, 'endpoint'), token: id(value.token, 'token') };
    default:
      throw new Error('Unknown IPC frame kind');
  }
}
