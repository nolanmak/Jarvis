import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import test from 'node:test';
import { mono24ToStereo48, resampleDiscordPcmForStt } from '../src/audio-format.js';

test('24 kHz mono PCM becomes 48 kHz stereo across odd chunk boundaries', async () => {
  const input = Buffer.from([0, 0, 0x10, 0, 0x20, 0, 0x30, 0]);
  const output: Buffer[] = [];
  for await (const chunk of Readable.from([input.subarray(0, 3), input.subarray(3, 5),
    input.subarray(5)]).pipe(mono24ToStereo48())) output.push(Buffer.from(chunk));
  const pcm = Buffer.concat(output);
  assert.equal(pcm.length, 32);
  for (let offset = 0; offset < pcm.length; offset += 4) {
    assert.equal(pcm.readInt16LE(offset), pcm.readInt16LE(offset + 2));
  }
  assert.equal(pcm.readInt16LE(0), 0);
  assert.equal(pcm.readInt16LE(8), 0x10);
  assert.equal(pcm.readInt16LE(16), 0x20);
  assert.equal(pcm.readInt16LE(24), 0x30);
});

test('Discord 48 kHz stereo PCM is resampled to 16 kHz mono PCM', async () => {
  const frames = 4800; // 100 ms
  const discord = Buffer.alloc(frames * 4);
  for (let index = 0; index < frames; index++) {
    const sample = Math.round(Math.sin(index * 2 * Math.PI * 440 / 48000) * 10000);
    discord.writeInt16LE(sample, index * 4);
    discord.writeInt16LE(sample, index * 4 + 2);
  }
  const resampler = resampleDiscordPcmForStt();
  const output: Buffer[] = [];
  const reading = (async () => {
    for await (const chunk of resampler.output) output.push(Buffer.from(chunk));
  })();
  resampler.input.end(discord);
  await reading;
  assert.equal(Buffer.concat(output).length, 1600 * 2);
  await resampler.close();
});
