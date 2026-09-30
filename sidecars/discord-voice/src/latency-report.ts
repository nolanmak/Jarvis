type Agent = 'codex' | 'claude';
type Provider = 'deepgram' | 'elevenlabs';
type Stage = 'speechEndedAtMs' | 'committedAtMs' | 'handlerDispatchedAtMs' | 'nativeSubmittedAtMs' |
  'firstTextOutputAtMs' | 'answerCompletedAtMs' | 'queuedAtMs' |
  'ttsRequestedAtMs' | 'ttsFirstByteAtMs' | 'firstPlaybackAtMs';

const STAGES: Stage[] = ['speechEndedAtMs', 'committedAtMs', 'handlerDispatchedAtMs', 'nativeSubmittedAtMs',
  'firstTextOutputAtMs', 'answerCompletedAtMs', 'queuedAtMs',
  'ttsRequestedAtMs', 'ttsFirstByteAtMs', 'firstPlaybackAtMs'];

export type Percentiles = { p50: number; p95: number };
export type AgentLatency = {
  samples: number;
  endpointingMs: Percentiles;
  commitToSubmitMs: Percentiles;
  agentWaitMs: Percentiles;
  agentTotalMs: Percentiles;
  ttsFirstByteMs: Percentiles;
  speakToLocalPlaybackMs: Percentiles;
  totalMs: Percentiles;
  gates: { commitToSubmit: boolean; speakToLocalPlayback: boolean };
};
export type LatencyReport = {
  schemaVersion: 1;
  evidence: 'operator-supplied-timings-unverified';
  deployment: string;
  network: string;
  sttProvider: Provider;
  ttsProvider: Provider;
  sampleCount: number;
  agents: Record<Agent, AgentLatency>;
  passed: boolean;
  note: string;
};

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Latency input must contain objects');
  }
  return value as Record<string, unknown>;
}

function label(value: unknown, name: string): string {
  if (typeof value !== 'string' || !value.trim() || value.length > 128 ||
      [...value].some(character => character.charCodeAt(0) < 32)) {
    throw new Error(`${name} is required and must be a short printable string`);
  }
  return value;
}

function provider(value: unknown): Provider {
  if (value !== 'deepgram' && value !== 'elevenlabs') {
    throw new Error('Latency input must identify each speech provider');
  }
  return value;
}

function percentiles(values: number[]): Percentiles {
  const sorted = [...values].sort((left, right) => left - right);
  return { p50: sorted[Math.ceil(sorted.length * 0.50) - 1]!,
    p95: sorted[Math.ceil(sorted.length * 0.95) - 1]! };
}

type Timing = Record<Stage, number> & { agent: Agent; turnId: string };

/** Strictly account for every supplied no-tool sample; never trim slow outliers. */
export function calculateLatencyReport(raw: unknown): LatencyReport {
  const input = record(raw);
  if (input.schemaVersion !== 1) throw new Error('Unsupported latency input schema');
  const deployment = label(input.deployment, 'deployment');
  const network = label(input.network, 'network');
  const sttProvider = provider(input.sttProvider);
  const ttsProvider = provider(input.ttsProvider);
  if (!Array.isArray(input.turns) || input.turns.length < 60 || input.turns.length > 1_000) {
    throw new Error('Provide at least 30 no-tool turns for each agent (60 total; max 1,000)');
  }
  const groups: Record<Agent, Timing[]> = { codex: [], claude: [] };
  const seen = new Set<string>();
  for (const [index, value] of input.turns.entries()) {
    const turn = record(value);
    if (turn.agent !== 'codex' && turn.agent !== 'claude') {
      throw new Error(`Turn ${index} has an invalid agent`);
    }
    const agent = turn.agent;
    const turnId = label(turn.turnId, `turn ${index} ID`);
    if (seen.has(`${agent}:${turnId}`)) throw new Error('Duplicate turn ID in latency input');
    seen.add(`${agent}:${turnId}`);
    if (turn.noTool !== true) throw new Error(`Turn ${index} is not a no-tool turn`);
    if (turn.idleAtCommit !== true) throw new Error(`Turn ${index} was not idle at transcript commit`);
    if (!Number.isSafeInteger(turn.speakTextChars) ||
        (turn.speakTextChars as number) < 1 || (turn.speakTextChars as number) > 200) {
      throw new Error(`Turn ${index} must have 1-200 spoken text characters`);
    }
    const stamps = {} as Record<Stage, number>;
    for (const stage of STAGES) {
      const stamp = turn[stage];
      if (!Number.isSafeInteger(stamp) || (stamp as number) < 0) {
        throw new Error(`Turn ${index} is missing a valid ${stage}`);
      }
      stamps[stage] = stamp as number;
    }
    for (let stage = 1; stage < STAGES.length; stage++) {
      if (stamps[STAGES[stage]!]! < stamps[STAGES[stage - 1]!]!) {
        throw new Error(`Turn ${index} timestamps are not chronological`);
      }
    }
    groups[agent].push({ agent, turnId, ...stamps });
  }
  if (groups.codex.length < 30 || groups.claude.length < 30) {
    throw new Error('Provide at least 30 no-tool turns for each agent');
  }
  const calculate = (turns: Timing[]): AgentLatency => {
    const duration = (start: Stage, end: Stage): Percentiles =>
      percentiles(turns.map(turn => turn[end] - turn[start]));
    const commitToSubmitMs = duration('committedAtMs', 'nativeSubmittedAtMs');
    const speakToLocalPlaybackMs = duration('queuedAtMs', 'firstPlaybackAtMs');
    return {
      samples: turns.length,
      endpointingMs: duration('speechEndedAtMs', 'committedAtMs'),
      commitToSubmitMs,
      agentWaitMs: duration('nativeSubmittedAtMs', 'firstTextOutputAtMs'),
      agentTotalMs: duration('nativeSubmittedAtMs', 'answerCompletedAtMs'),
      ttsFirstByteMs: duration('ttsRequestedAtMs', 'ttsFirstByteAtMs'),
      speakToLocalPlaybackMs,
      totalMs: duration('speechEndedAtMs', 'firstPlaybackAtMs'),
      gates: { commitToSubmit: commitToSubmitMs.p95 <= 250,
        speakToLocalPlayback: speakToLocalPlaybackMs.p95 <= 1_500 },
    };
  };
  const agents = { codex: calculate(groups.codex), claude: calculate(groups.claude) };
  return {
    schemaVersion: 1, evidence: 'operator-supplied-timings-unverified',
    deployment, network, sttProvider, ttsProvider, sampleCount: input.turns.length,
    agents, passed: Object.values(agents).every(agent =>
      agent.gates.commitToSubmit && agent.gates.speakToLocalPlayback),
    note: 'First playback is the local Discord audio-player transition, not proof of remote audibility.',
  };
}
