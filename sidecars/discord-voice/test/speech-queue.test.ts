import assert from 'node:assert/strict';
import test from 'node:test';
import { SpeechQueue } from '../src/speech-queue.js';

test('speech starts before synthesis completes and duplicate receipt never replays', async () => {
  let releaseFinal!: () => void;
  const final = new Promise<void>(resolve => { releaseFinal = resolve; });
  let firstAudio!: () => void;
  const first = new Promise<void>(resolve => { firstAudio = resolve; });
  let synthesisCalls = 0;
  const played: number[] = [];
  const queue = new SpeechQueue(
    async function* () {
      synthesisCalls++;
      yield Buffer.from([1, 0]);
      await final;
      yield Buffer.from([2, 0]);
    },
    { async play(audio) {
      for await (const chunk of audio) {
        played.push(chunk.readInt16LE(0));
        if (played.length === 1) firstAudio();
      }
    }, interrupt() {} },
  );
  const receipt = queue.speak('utterance-1', 'hello');
  assert.equal(receipt.status, 'queued');
  await first;
  assert.equal(queue.status('utterance-1')?.status, 'playing');
  assert.deepEqual(played, [1]);
  assert.equal(queue.speak('utterance-1', 'hello').utteranceId, 'utterance-1');
  releaseFinal();
  await queue.whenDone('utterance-1');
  assert.deepEqual(played, [1, 2]);
  assert.equal(queue.status('utterance-1')?.status, 'completed');
  assert.equal(synthesisCalls, 1);
});

test('interrupt stops playback and drops every late synthesis chunk', async () => {
  let releaseLate!: () => void;
  const late = new Promise<void>(resolve => { releaseLate = resolve; });
  let firstAudio!: () => void;
  const first = new Promise<void>(resolve => { firstAudio = resolve; });
  const played: number[] = [];
  let stops = 0;
  const queue = new SpeechQueue(
    async function* () {
      yield Buffer.from([1, 0]);
      await late;
      yield Buffer.from([2, 0]);
    },
    { async play(audio) {
      for await (const chunk of audio) {
        played.push(chunk.readInt16LE(0));
        firstAudio();
      }
    }, interrupt() { stops++; } },
  );
  queue.speak('utterance-2', 'hello');
  await first;
  queue.interrupt();
  releaseLate();
  await queue.whenDone('utterance-2');
  assert.equal(queue.status('utterance-2')?.status, 'interrupted');
  assert.deepEqual(played, [1]);
  assert.equal(stops, 1);
});

test('twenty overlap cases stop local playback promptly and fence late chunks', async () => {
  for (let overlap = 0; overlap < 20; overlap++) {
    let releaseLate!: () => void;
    const late = new Promise<void>(resolve => { releaseLate = resolve; });
    let firstAudio!: () => void;
    const first = new Promise<void>(resolve => { firstAudio = resolve; });
    const played: number[] = [];
    let stoppedAt = 0;
    const queue = new SpeechQueue(async function* (text) {
      if (text === 'pending') throw new Error('pending synthesis must never start');
      yield Buffer.from([overlap, 0]);
      await late;
      yield Buffer.from([100 + overlap, 0]);
    }, { async play(audio) {
      for await (const chunk of audio) {
        played.push(chunk.readInt16LE(0));
        if (played.length === 1) firstAudio();
      }
    }, interrupt() { stoppedAt = performance.now(); } });
    const activeId = `active-${overlap}`;
    queue.speak(activeId, 'active');
    await first;
    const pendingId = `pending-${overlap}`;
    if (overlap % 2 === 1) queue.speak(pendingId, 'pending');
    const eventAt = performance.now();
    queue.interrupt(); // owner speech-start and /voice interrupt use this boundary
    assert.ok(stoppedAt - eventAt < 250, `overlap ${overlap} did not stop within 250 ms`);
    releaseLate();
    assert.equal((await queue.whenDone(activeId)).status, 'interrupted');
    if (overlap % 2 === 1) {
      assert.equal((await queue.whenDone(pendingId)).status, 'interrupted');
    }
    assert.deepEqual(played, [overlap], `overlap ${overlap} played a stale chunk`);
  }
});
