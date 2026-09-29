import assert from 'node:assert/strict';
import test from 'node:test';
import { calculateLatencyReport } from '../src/latency-report.js';

function fixture(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  const turns = (['codex', 'claude'] as const).flatMap(agent =>
    Array.from({ length: 30 }, (_, index) => {
      const base = 1_000_000 + index * 10_000;
      return { agent, turnId: `${agent}-${index}`, noTool: true, idleAtCommit: true,
        speakTextChars: 90, speechEndedAtMs: base,
        committedAtMs: base + 100, handlerDispatchedAtMs: base + 150,
        nativeSubmittedAtMs: base + 200,
        firstTextOutputAtMs: base + 700, answerCompletedAtMs: base + 900,
        queuedAtMs: base + 950, ttsRequestedAtMs: base + 960,
        ttsFirstByteAtMs: base + 1_100, firstPlaybackAtMs: base + 1_300 };
    }));
  return { schemaVersion: 1, deployment: 'synthetic-linux', network: 'synthetic-loopback',
    sttProvider: 'deepgram', ttsProvider: 'deepgram', turns, ...overrides };
}

test('complete 30-turn-per-agent input reports all stages and both passing p95 gates', () => {
  const report = calculateLatencyReport(fixture());
  assert.equal(report.sampleCount, 60);
  for (const agent of ['codex', 'claude'] as const) {
    assert.equal(report.agents[agent].samples, 30);
    assert.equal(report.agents[agent].endpointingMs.p95, 100);
    assert.equal(report.agents[agent].agentWaitMs.p95, 500);
    assert.equal(report.agents[agent].ttsFirstByteMs.p95, 140);
    assert.equal(report.agents[agent].speakToLocalPlaybackMs.p95, 350);
    assert.equal(report.agents[agent].totalMs.p95, 1_300);
    assert.equal(report.agents[agent].gates.commitToSubmit, true);
    assert.equal(report.agents[agent].gates.speakToLocalPlayback, true);
  }
  assert.equal(report.passed, true);
  assert.equal(report.evidence, 'operator-supplied-timings-unverified');
});

test('a slow sample stays in the p95 calculation and fails the stated gate', () => {
  const input = fixture();
  const turns = input.turns as Array<Record<string, unknown>>;
  for (const index of [27, 28, 29]) {
    const row = turns[index]!;
    row.nativeSubmittedAtMs = (row.committedAtMs as number) + 300;
    row.firstTextOutputAtMs = (row.firstTextOutputAtMs as number) + 200;
    row.answerCompletedAtMs = (row.answerCompletedAtMs as number) + 200;
    row.queuedAtMs = (row.queuedAtMs as number) + 200;
    row.ttsRequestedAtMs = (row.ttsRequestedAtMs as number) + 200;
    row.ttsFirstByteAtMs = (row.ttsFirstByteAtMs as number) + 200;
    row.firstPlaybackAtMs = (row.firstPlaybackAtMs as number) + 200;
  }
  const report = calculateLatencyReport(input);
  assert.equal(report.agents.codex.commitToSubmitMs.p95, 300);
  assert.equal(report.agents.codex.gates.commitToSubmit, false);
  assert.equal(report.agents.codex.samples, 30);
  assert.equal(report.passed, false);
});

test('incomplete, duplicate, implausible, or overlong samples cannot pass', () => {
  const fewer = fixture();
  (fewer.turns as unknown[]).pop();
  assert.throws(() => calculateLatencyReport(fewer), /30/);
  const duplicate = fixture();
  (duplicate.turns as Array<Record<string, unknown>>)[1]!.turnId = 'codex-0';
  assert.throws(() => calculateLatencyReport(duplicate), /Duplicate/);
  const inverted = fixture();
  (inverted.turns as Array<Record<string, unknown>>)[0]!.committedAtMs = 1;
  assert.throws(() => calculateLatencyReport(inverted), /chronological/);
  const long = fixture();
  (long.turns as Array<Record<string, unknown>>)[0]!.speakTextChars = 201;
  assert.throws(() => calculateLatencyReport(long), /200/);
  const busy = fixture();
  (busy.turns as Array<Record<string, unknown>>)[0]!.idleAtCommit = false;
  assert.throws(() => calculateLatencyReport(busy), /idle/);
});
