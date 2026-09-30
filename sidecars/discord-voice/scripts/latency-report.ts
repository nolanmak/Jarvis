import { readFile, writeFile } from 'node:fs/promises';
import { calculateLatencyReport } from '../src/latency-report.js';

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  if (args.length !== 3 || !args[0] || args[1] !== '--output' || !args[2]) {
    throw new Error('Usage: npm run report:latency -- <timings.json> --output <new-report.json>');
  }
  const inputPath = args[0]!;
  const outputPath = args[2]!;
  if (inputPath === outputPath) throw new Error('Input and output paths must differ');
  const report = calculateLatencyReport(JSON.parse(await readFile(inputPath, 'utf8')));
  await writeFile(outputPath, `${JSON.stringify(report, null, 2)}\n`, { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify({ output: outputPath, evidence: report.evidence,
    samples: report.sampleCount, passed: report.passed, agents: report.agents }));
  if (!report.passed) process.exitCode = 1;
}

main().catch(error => {
  console.error(error instanceof Error ? error.message : 'Latency report failed');
  process.exitCode = 1;
});
