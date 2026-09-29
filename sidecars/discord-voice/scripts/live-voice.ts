import { writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline/promises';
import { checkVoicePreflight } from '../src/doctor.js';
import { parseLiveArgs } from '../src/live-config.js';

try {
  const config = parseLiveArgs(process.argv.slice(2));
  const preflight = await checkVoicePreflight(process.env);
  const startedAt = new Date().toISOString();
  console.log(JSON.stringify({ status: 'awaiting-live-evidence', startedAt,
    test: config, preflight }));
  if (!config.preflightOnly) {
    if (!process.stdin.isTTY || !config.output) {
      throw new Error('Interactive live recording requires a terminal and --output <new JSON file>');
    }
    const input = createInterface({ input: process.stdin, output: process.stdout });
    try {
      console.log(`In Discord, join voice channel ${config.voiceChannelId} as the owner.`);
      console.log(`From text channel/thread ${config.textChannelId}, run /voice start for ${config.agent}.`);
      const nativeSessionId = (await input.question('Native session ID shown by /voice start: ')).trim();
      if (!nativeSessionId || nativeSessionId.length > 128) throw new Error('A native session ID is required');
      console.log('Speak a disposable marker phrase, wait for the mirrored transcript and audible reply, then use /voice stop.');
      const marker = (await input.question('Disposable phrase spoken: ')).trim();
      const transcript = (await input.question('Exact mirrored transcript observed: ')).trim();
      const heardReply = (await input.question('Did you hear the bot reply? Type yes or no: ')).trim();
      const detached = (await input.question('Did /voice stop detach audio? Type yes or no: ')).trim();
      const textContinues = (await input.question('Did typed text continue in the same native session? Type yes or no: ')).trim();
      if (!marker || !transcript || !['yes', 'no'].includes(heardReply) ||
          !['yes', 'no'].includes(detached) || !['yes', 'no'].includes(textContinues)) {
        throw new Error('Complete every observation with a phrase/transcript or yes/no');
      }
      const record = { status: 'operator-recorded-unverified', startedAt,
        finishedAt: new Date().toISOString(), test: config, preflight,
        observations: { nativeSessionId, marker, transcript,
          heardReply: heardReply === 'yes', detached: detached === 'yes',
          textContinues: textContinues === 'yes' } };
      await writeFile(config.output, `${JSON.stringify(record, null, 2)}\n`, { flag: 'wx', mode: 0o600 });
      console.log(`Recorded observations in ${config.output}; review with Discord/session traces before marking an acceptance gate passed.`);
    } finally {
      input.close();
    }
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : 'Live verification failed');
  process.exitCode = 1;
}
