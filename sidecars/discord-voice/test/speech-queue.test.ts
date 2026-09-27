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
