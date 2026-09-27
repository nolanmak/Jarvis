export type SpeechStatus = 'queued' | 'playing' | 'completed' | 'interrupted' | 'failed';
export type SpeechReceipt = {
  utteranceId: string;
  status: SpeechStatus;
  queuedAtMs: number;
  ttsRequestedAtMs?: number;
  ttsFirstByteAtMs?: number;
  firstPlaybackAtMs?: number;
  stoppedAtMs?: number;
  error?: string;
};
export type Synthesize = (text: string, signal: AbortSignal) => AsyncIterable<Buffer>;
export type PlaybackSink = {
  play(audio: AsyncIterable<Buffer>, onPlaybackStart?: () => void): Promise<void>;
  interrupt(): void;
};

const MAX_PENDING = 10;
const MAX_RECEIPTS = 10_000;

type Job = {
  text: string;
  receipt: SpeechReceipt;
  controller: AbortController;
  done: Promise<SpeechReceipt>;
  resolve(receipt: SpeechReceipt): void;
};

/** Serializes speech output and fences late chunks after an interruption. */
export class SpeechQueue {
  private readonly receipts = new Map<string, Job>();
  private readonly pending: Job[] = [];
  private active?: Job;
  private running = false;
  private generation = 0;
  private closed = false;

  constructor(private readonly synthesize: Synthesize, private readonly sink: PlaybackSink) {}

  speak(utteranceId: string, text: string): SpeechReceipt {
    const existing = this.receipts.get(utteranceId);
    if (existing) return existing.receipt;
    if (this.closed) throw new Error('Speech queue is stopped');
    if (!utteranceId || !text.trim() || text.length > 12_000) throw new Error('Invalid speech request');
    if (this.pending.length >= MAX_PENDING) throw new Error('Speech queue is full (10 pending utterances)');
    if (this.receipts.size >= MAX_RECEIPTS) throw new Error('Speech receipt limit reached');
    const receipt: SpeechReceipt = { utteranceId, status: 'queued', queuedAtMs: Date.now() };
    let resolve!: (value: SpeechReceipt) => void;
    const done = new Promise<SpeechReceipt>(done => { resolve = done; });
    const job: Job = { text, receipt, controller: new AbortController(), done, resolve };
    this.receipts.set(utteranceId, job);
    this.pending.push(job);
    queueMicrotask(() => { void this.pump(); });
    return receipt;
  }

  status(utteranceId: string): SpeechReceipt | undefined {
    return this.receipts.get(utteranceId)?.receipt;
  }

  async whenDone(utteranceId: string): Promise<SpeechReceipt> {
    const job = this.receipts.get(utteranceId);
    if (!job) throw new Error('Unknown speech receipt');
    return job.done;
  }

  interrupt(): void {
    this.generation++;
    if (this.active) {
      this.active.receipt.status = 'interrupted';
      this.active.receipt.stoppedAtMs ??= Date.now();
      this.active.controller.abort();
      this.sink.interrupt();
    }
    for (const job of this.pending.splice(0)) {
      job.receipt.status = 'interrupted';
      job.receipt.stoppedAtMs = Date.now();
      job.controller.abort();
      job.resolve(job.receipt);
    }
  }

  stop(): void {
    if (this.closed) return;
    this.closed = true;
    this.interrupt();
  }

  private async *currentChunks(job: Job, generation: number): AsyncGenerator<Buffer> {
    for await (const chunk of this.synthesize(job.text, job.controller.signal)) {
      if (job.controller.signal.aborted || generation !== this.generation) return;
      if (chunk.length && job.receipt.ttsFirstByteAtMs === undefined) {
        job.receipt.ttsFirstByteAtMs = Date.now();
      }
      yield chunk;
    }
  }

  private async pump(): Promise<void> {
    if (this.running) return;
    this.running = true;
    try {
      while (!this.closed && this.pending.length) {
        const job = this.pending.shift()!;
        this.active = job;
        const generation = this.generation;
        job.receipt.status = 'playing';
        job.receipt.ttsRequestedAtMs = Date.now();
        try {
          await this.sink.play(this.currentChunks(job, generation), () => {
            job.receipt.firstPlaybackAtMs ??= Date.now();
          });
          if (job.receipt.status === 'playing') job.receipt.status = 'completed';
        } catch (error) {
          if (!job.controller.signal.aborted) {
            job.receipt.status = 'failed';
            job.receipt.error = error instanceof Error ? error.message : 'Speech playback failed';
          }
        } finally {
          job.receipt.stoppedAtMs ??= Date.now();
          job.resolve(job.receipt);
          this.active = undefined;
        }
      }
    } finally {
      this.running = false;
      if (!this.closed && this.pending.length) queueMicrotask(() => { void this.pump(); });
    }
  }
}
