import { checkVoicePreflight } from '../src/doctor.js';

try {
  const result = await checkVoicePreflight(process.env);
  console.log(JSON.stringify({ status: 'ready-for-live-test', ...result }));
} catch (error) {
  console.error(JSON.stringify({ status: 'failed', reason:
    error instanceof Error ? error.message : 'Unknown preflight error' }));
  process.exitCode = 1;
}
