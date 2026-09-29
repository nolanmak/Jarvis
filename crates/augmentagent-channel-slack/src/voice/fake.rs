//! Scripted, offline speech providers for tests and the debug-build CLI QA
//! override. They never touch the network. Each call takes the next scripted
//! step; the last step repeats.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::audio::{wav_duration_ms, wav_from_pcm16};
use super::speech::{
    AudioFormat, SpeechError, SpeechOperation, SpeechToText, SynthesizedAudio, TextToSpeech,
};

/// One scripted STT step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttScript {
    Text(String),
    /// Fail with this provider code (`402`, `quota_exceeded`, `500`, …).
    Fail(String),
    /// Never answer (for timeout and cancellation tests).
    Hang,
}

/// What the fake STT was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenClip {
    pub sample_rate: u32,
    pub duration_ms: u64,
    pub bytes: usize,
}

#[derive(Debug)]
struct Steps<T: Clone> {
    queue: VecDeque<T>,
    last: Option<T>,
}

impl<T: Clone> Steps<T> {
    fn new(steps: Vec<T>) -> Self {
        Self {
            queue: steps.into(),
            last: None,
        }
    }

    fn next(&mut self) -> Option<T> {
        if let Some(s) = self.queue.pop_front() {
            self.last = Some(s.clone());
            return Some(s);
        }
        self.last.clone()
    }
}

/// Scripted [`SpeechToText`]; clones share the script and the record.
#[derive(Debug, Clone)]
pub struct ScriptedStt {
    name: String,
    steps: Arc<Mutex<Steps<SttScript>>>,
    seen: Arc<Mutex<Vec<SeenClip>>>,
}

impl ScriptedStt {
    pub fn new(name: &str, steps: Vec<SttScript>) -> Self {
        Self {
            name: name.to_string(),
            steps: Arc::new(Mutex::new(Steps::new(steps))),
            seen: Arc::default(),
        }
    }

    /// Every clip handed to this provider, in order.
    pub fn seen(&self) -> Vec<SeenClip> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl SpeechToText for ScriptedStt {
    fn provider(&self) -> &str {
        &self.name
    }

    async fn transcribe(&self, wav: &Path) -> Result<String, SpeechError> {
        let bytes = std::fs::read(wav)
            .map_err(|e| SpeechError::new(&self.name, SpeechOperation::Stt, "io", e.to_string()))?;
        let (duration_ms, sample_rate) = wav_duration_ms(&bytes).unwrap_or((0, 0));
        self.seen.lock().unwrap().push(SeenClip {
            sample_rate,
            duration_ms,
            bytes: bytes.len(),
        });
        let step = self.steps.lock().unwrap().next();
        match step {
            Some(SttScript::Text(t)) => Ok(t),
            Some(SttScript::Fail(code)) => Err(SpeechError::new(
                &self.name,
                SpeechOperation::Stt,
                code.clone(),
                format!("scripted failure {code}"),
            )),
            Some(SttScript::Hang) => std::future::pending().await,
            None => Err(SpeechError::new(
                &self.name,
                SpeechOperation::Stt,
                "unscripted",
                "the fake has no scripted answer",
            )),
        }
    }
}

/// One scripted TTS step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsScript {
    /// A WAV of this many milliseconds (16 kHz tone).
    Wav(u64),
    /// Raw 24 kHz PCM of this many milliseconds, like the sidecar's clients.
    Pcm24k(u64),
    Fail(String),
}

/// Scripted [`TextToSpeech`]; clones share the script and the record.
#[derive(Debug, Clone)]
pub struct ScriptedTts {
    name: String,
    steps: Arc<Mutex<Steps<TtsScript>>>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl ScriptedTts {
    pub fn new(name: &str, steps: Vec<TtsScript>) -> Self {
        Self {
            name: name.to_string(),
            steps: Arc::new(Mutex::new(Steps::new(steps))),
            seen: Arc::default(),
        }
    }

    /// Every text this provider was asked to speak.
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn tone(ms: u64, rate: u32) -> Vec<i16> {
    let n = (rate as u64 * ms / 1000) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / rate as f32;
            ((t * 330.0 * std::f32::consts::TAU).sin() * 2000.0) as i16
        })
        .collect()
}

#[async_trait]
impl TextToSpeech for ScriptedTts {
    fn provider(&self) -> &str {
        &self.name
    }

    async fn synthesize(&self, text: &str) -> Result<SynthesizedAudio, SpeechError> {
        self.seen.lock().unwrap().push(text.to_string());
        let step = self.steps.lock().unwrap().next();
        match step {
            Some(TtsScript::Wav(ms)) => Ok(SynthesizedAudio {
                bytes: wav_from_pcm16(&tone(ms, 16_000), 16_000),
                format: AudioFormat::Wav,
            }),
            Some(TtsScript::Pcm24k(ms)) => Ok(SynthesizedAudio {
                bytes: tone(ms, 24_000)
                    .iter()
                    .flat_map(|s| s.to_le_bytes())
                    .collect(),
                format: AudioFormat::Pcm16 {
                    sample_rate: 24_000,
                },
            }),
            Some(TtsScript::Fail(code)) => Err(SpeechError::new(
                &self.name,
                SpeechOperation::Tts,
                code.clone(),
                format!("scripted failure {code}"),
            )),
            None => Err(SpeechError::new(
                &self.name,
                SpeechOperation::Tts,
                "unscripted",
                "the fake has no scripted answer",
            )),
        }
    }
}
