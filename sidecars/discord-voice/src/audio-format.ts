import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { Transform, type TransformCallback, type Readable, type Writable } from 'node:stream';

const ffmpegPath = createRequire(import.meta.url)('ffmpeg-static') as string | null;

/** Convert raw mono signed 16-bit PCM at 24 kHz to Discord's stereo 48 kHz. */
export function mono24ToStereo48(): Transform {
  let previous: number | undefined;
  let carry: number | undefined;
  return new Transform({
    transform(chunk: Buffer, _encoding: BufferEncoding, callback: TransformCallback) {
      const bytes = carry === undefined ? chunk : Buffer.concat([Buffer.from([carry]), chunk]);
      carry = bytes.length % 2 ? bytes[bytes.length - 1] : undefined;
      const samples = Math.floor(bytes.length / 2);
      const output = Buffer.allocUnsafe(samples * 8);
      let written = 0;
      for (let index = 0; index < samples; index++) {
        const current = bytes.readInt16LE(index * 2);
        if (previous !== undefined) {
          const midpoint = Math.round((previous + current) / 2);
          for (const sample of [previous, midpoint]) {
            output.writeInt16LE(sample, written);
            output.writeInt16LE(sample, written + 2);
            written += 4;
          }
        }
        previous = current;
      }
      if (written) this.push(output.subarray(0, written));
      callback();
    },
    flush(callback: TransformCallback) {
      if (carry !== undefined) return callback(new Error('TTS PCM ended on a partial sample'));
      if (previous !== undefined) {
        const output = Buffer.allocUnsafe(8);
        for (const offset of [0, 2, 4, 6]) output.writeInt16LE(previous, offset);
        this.push(output);
      }
      callback();
    },
  });
}

export type SttResampler = {
  input: Writable;
  output: Readable;
  close(): Promise<void>;
  abort(): void;
};

/** FFmpeg applies the anti-alias filter required for 48 kHz stereo -> 16 kHz mono. */
export function resampleDiscordPcmForStt(): SttResampler {
  if (!ffmpegPath) throw new Error('Pinned FFmpeg binary is unavailable');
  const child = spawn(ffmpegPath, [
    '-hide_banner', '-loglevel', 'error',
    '-probesize', '32', '-analyzeduration', '0',
    '-f', 's16le', '-ar', '48000', '-ac', '2', '-i', 'pipe:0',
    '-f', 's16le', '-ar', '16000', '-ac', '1', 'pipe:1',
  ], { stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '';
  let aborted = false;
  let forceKill: ReturnType<typeof setTimeout> | undefined;
  child.stderr.on('data', (chunk: Buffer) => { stderr = (stderr + chunk.toString()).slice(-4096); });
  const exit = new Promise<void>((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => {
      if (forceKill) clearTimeout(forceKill);
      if (aborted || code === 0 || signal === 'SIGTERM') resolve();
      else reject(new Error(`FFmpeg resampling failed (${code ?? signal}): ${stderr}`));
    });
  });
  return {
    input: child.stdin,
    output: child.stdout,
    async close(): Promise<void> {
      if (!child.stdin.writableEnded) child.stdin.end();
      await exit;
    },
    abort(): void {
      if (aborted) return;
      aborted = true;
      child.stdin.destroy();
      child.stdout.destroy();
      child.stderr.destroy();
      child.kill('SIGTERM');
      forceKill = setTimeout(() => child.kill('SIGKILL'), 200);
      forceKill.unref();
    },
  };
}
