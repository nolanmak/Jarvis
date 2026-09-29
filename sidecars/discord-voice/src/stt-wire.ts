export type SttProvider = 'deepgram' | 'elevenlabs';
export type SttEvent =
  | { kind: 'speech_start'; turnIndex: number }
  | { kind: 'partial'; text: string; turnIndex?: number }
  | { kind: 'final'; text: string; turnIndex?: number };

/** Only provider-committed text may enter the native conversation scheduler. */
export function parseSttEvent(provider: SttProvider, raw: string): SttEvent | null {
  let value: unknown;
  try { value = JSON.parse(raw) as unknown; } catch { return null; }
  if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
  const message = value as Record<string, unknown>;
  if (provider === 'deepgram') {
    if (message.type !== 'TurnInfo' || !Number.isSafeInteger(message.turn_index) ||
        (message.turn_index as number) < 0) return null;
    const turnIndex = message.turn_index as number;
    if (message.event === 'StartOfTurn') return { kind: 'speech_start', turnIndex };
    if (typeof message.transcript !== 'string' || !message.transcript.trim()) return null;
    if (message.event === 'Update') return { kind: 'partial', text: message.transcript.trim(), turnIndex };
    if (message.event === 'EndOfTurn') return { kind: 'final', text: message.transcript.trim(), turnIndex };
    return null;
  }
  if (typeof message.text !== 'string' || !message.text.trim()) return null;
  if (message.message_type === 'partial_transcript') return { kind: 'partial', text: message.text.trim() };
  if (message.message_type === 'committed_transcript') return { kind: 'final', text: message.text.trim() };
  return null;
}
