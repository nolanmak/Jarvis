import type { SttProvider } from './stt-wire.js';

/** Only confirmed credit exhaustion triggers a vendor switch. */
export class ProviderError extends Error {
  constructor(
    public readonly provider: SttProvider,
    public readonly operation: 'STT' | 'TTS',
    public readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = 'ProviderError';
  }
  get exhausted(): boolean { return this.code === '402' || this.code === 'quota_exceeded'; }
}
