import assert from 'node:assert/strict';
import test from 'node:test';
import { loadSpeechConfig } from '../src/speech-runtime.js';

for (const stt of ['deepgram', 'elevenlabs'] as const) {
  for (const tts of ['deepgram', 'elevenlabs'] as const) {
    test(`independent provider pair ${stt} STT + ${tts} TTS`, () => {
      const config = loadSpeechConfig({
        AUGMENTAGENT_DISCORD_STT_PROVIDER: stt,
        AUGMENTAGENT_DISCORD_TTS_PROVIDER: tts,
        DEEPGRAM_API_KEY: 'synthetic-deepgram-key',
        ELEVENLABS_API_KEY: 'synthetic-elevenlabs-key',
        ELEVENLABS_VOICE_ID: 'synthetic-voice-id',
      });
      assert.equal(config.sttProvider, stt);
      assert.equal(config.ttsProvider, tts);
      assert.equal(config.sttKey, stt === 'deepgram' ? 'synthetic-deepgram-key' : 'synthetic-elevenlabs-key');
      assert.equal(config.ttsKey, tts === 'deepgram' ? 'synthetic-deepgram-key' : 'synthetic-elevenlabs-key');
    });
  }
}

test('missing selected credentials fail before joining voice', () => {
  assert.throws(() => loadSpeechConfig({}), /Deepgram STT key/);
  assert.throws(() => loadSpeechConfig({ DEEPGRAM_API_KEY: 'synthetic-deepgram-key',
    AUGMENTAGENT_DISCORD_TTS_PROVIDER: 'elevenlabs' }), /ElevenLabs TTS key/);
});
