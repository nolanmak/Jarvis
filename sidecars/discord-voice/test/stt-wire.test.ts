import assert from 'node:assert/strict';
import test from 'node:test';
import { parseSttEvent } from '../src/stt-wire.js';

test('Deepgram Flux commits only EndOfTurn and surfaces StartOfTurn for barge-in', () => {
  assert.deepEqual(parseSttEvent('deepgram', JSON.stringify({ type: 'TurnInfo', event: 'StartOfTurn',
    turn_index: 4, transcript: 'hello' })), { kind: 'speech_start', turnIndex: 4 });
  assert.deepEqual(parseSttEvent('deepgram', JSON.stringify({ type: 'TurnInfo', event: 'Update',
    turn_index: 4, transcript: 'hello wor' })), { kind: 'partial', text: 'hello wor', turnIndex: 4 });
  assert.deepEqual(parseSttEvent('deepgram', JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn',
    turn_index: 4, transcript: 'hello world' })), { kind: 'final', text: 'hello world', turnIndex: 4 });
  assert.equal(parseSttEvent('deepgram', JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn',
    turn_index: 4, transcript: '   ' })), null);
});

test('ElevenLabs Scribe commits only committed transcripts', () => {
  assert.deepEqual(parseSttEvent('elevenlabs', JSON.stringify({ message_type: 'partial_transcript',
    text: 'good mor' })), { kind: 'partial', text: 'good mor' });
  assert.deepEqual(parseSttEvent('elevenlabs', JSON.stringify({ message_type: 'committed_transcript',
    text: 'good morning' })), { kind: 'final', text: 'good morning' });
  assert.equal(parseSttEvent('elevenlabs', JSON.stringify({ message_type: 'session_started' })), null);
});

test('unknown or malformed provider events cannot become agent turns', () => {
  assert.equal(parseSttEvent('deepgram', '{broken'), null);
  assert.equal(parseSttEvent('elevenlabs', JSON.stringify({ message_type: 'rate_limited',
    error: 'limited' })), null);
  assert.equal(parseSttEvent('deepgram', JSON.stringify({ type: 'TurnInfo', event: 'EndOfTurn',
    turn_index: -1, transcript: 'bad' })), null);
});
