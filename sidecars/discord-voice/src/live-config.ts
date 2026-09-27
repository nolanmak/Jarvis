export type LiveConfig = {
  guildId: string;
  textChannelId: string;
  voiceChannelId: string;
  agent: 'codex' | 'claude';
  runId: string;
  output?: string;
  preflightOnly: boolean;
};

const SNOWFLAKE = /^[0-9]{15,20}$/;
const RUN_ID = /^[A-Za-z0-9_-]{1,64}$/;

export function parseLiveArgs(args: string[]): LiveConfig {
  const values = new Map<string, string>();
  let preflightOnly = false;
  for (let index = 0; index < args.length; index++) {
    const key = args[index];
    if (key === '--preflight-only') {
      preflightOnly = true;
      continue;
    }
    if (!key || !['--guild-id', '--text-channel-id', '--voice-channel-id',
      '--agent', '--run-id', '--output'].includes(key) || values.has(key)) {
      throw new Error(`Unknown or duplicate live-test argument: ${key ?? '(none)'}`);
    }
    const value = args[++index];
    if (!value || value.startsWith('--')) throw new Error(`Missing value for ${key}`);
    values.set(key, value);
  }
  const guildId = values.get('--guild-id');
  const textChannelId = values.get('--text-channel-id');
  const voiceChannelId = values.get('--voice-channel-id');
  const agent = values.get('--agent');
  const runId = values.get('--run-id');
  if (!guildId || !SNOWFLAKE.test(guildId) || !textChannelId || !SNOWFLAKE.test(textChannelId) ||
      !voiceChannelId || !SNOWFLAKE.test(voiceChannelId)) {
    throw new Error('Provide numeric test guild, text channel, and voice channel IDs');
  }
  if (agent !== 'codex' && agent !== 'claude') {
    throw new Error('Choose --agent codex or --agent claude');
  }
  if (!runId || !RUN_ID.test(runId)) {
    throw new Error('Provide a disposable --run-id (1-64 letters, digits, _ or -)');
  }
  return { guildId, textChannelId, voiceChannelId, agent, runId,
    output: values.get('--output'), preflightOnly };
}
