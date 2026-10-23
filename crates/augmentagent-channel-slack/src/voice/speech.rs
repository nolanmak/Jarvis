//! The speech seam: file-based speech-to-text and text-to-speech behind two
//! traits, with the provider fallback rule of the Discord voice sidecar.
//!
//! The streaming Deepgram/ElevenLabs clients live only in the TypeScript
//! sidecar (`sidecars/discord-voice/src/{stt,tts}.ts`), bound to a live
//! Discord session over private IPC; there is no Rust client and no
//! request/response call for a recorded clip. This module is therefore the
//! smallest shared Rust seam rather than a second provider stack:
//!
//! - [`SpeechToText`] / [`TextToSpeech`] take a finished file or text. The
//!   existing Rust transcriber, `augmentagent_channel_voice::Transcriber`
//!   (whisper.cpp, used for Telegram voice memos), plugs in through
//!   [`TranscriberStt`].
//! - [`SpeechError::exhausted`] and [`SttStack`] / [`TtsStack`] copy the
//!   sidecar's rule (`provider-error.ts`, `streamTtsWithFallback`): switch
//!   to the alternate vendor **only** on confirmed credit exhaustion (`402`
//!   or `quota_exceeded`); every other failure is reported as it is.
//! - Provider names are the sidecar's (`deepgram`, `elevenlabs`), so a
//!   future adapter that reaches the sidecar keeps the same selection.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use super::audio::wav_from_pcm16_bytes;

/// Which half of the speech stack failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechOperation {
    Stt,
    Tts,
}

impl SpeechOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            SpeechOperation::Stt => "STT",
            SpeechOperation::Tts => "TTS",
        }
    }
}

/// A provider failure. `code` is the provider's HTTP status or error code
/// (`402`, `quota_exceeded`, `500`, `timeout`, …); `message` is owner-safe
/// (no keys, no URLs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechError {
    pub provider: String,
    pub operation: SpeechOperation,
    pub code: String,
    pub message: String,
}

/// `code` of a transcript with no words in it.
pub const NO_SPEECH: &str = "no_speech";
/// `code` when no provider is configured for the operation.
pub const NOT_CONFIGURED: &str = "not_configured";

impl SpeechError {
    pub fn new(
        provider: impl Into<String>,
        operation: SpeechOperation,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            operation,
            code: code.into(),
            message: message.into(),
        }
    }

    /// Confirmed credit exhaustion: the only failure that switches vendor
    /// (same test as the sidecar's `ProviderError.exhausted`).
    pub fn exhausted(&self) -> bool {
        self.code == "402" || self.code == "quota_exceeded"
    }
}

impl std::fmt::Display for SpeechError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} failed ({}): {}",
            self.provider,
            self.operation.as_str(),
            self.code,
            self.message
        )
    }
}

impl std::error::Error for SpeechError {}

/// Speech-to-text for one recorded clip, already decoded to 16 kHz mono
/// 16-bit WAV (the format the sidecar streams to both vendors).
#[async_trait]
pub trait SpeechToText: Send + Sync {
    /// `deepgram`, `elevenlabs`, `whisper-cpp`, … (shown to the owner).
    fn provider(&self) -> &str;
    /// The transcript; may be empty when nothing was said.
    async fn transcribe(&self, wav: &Path) -> Result<String, SpeechError>;
    /// `Err(reason)` when this provider cannot run on this host at all, so
    /// a clip is refused before it is downloaded.
    fn readiness(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Container of synthesised audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Wav,
    Mp3,
    /// Raw mono signed 16-bit little-endian PCM (what the sidecar's TTS
    /// clients yield, at 24 kHz); wrapped into a WAV before upload.
    Pcm16 {
        sample_rate: u32,
    },
}

/// One synthesised reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedAudio {
    pub bytes: Vec<u8>,
    pub format: AudioFormat,
}

/// Largest synthesised file accepted for upload.
pub const MAX_SPOKEN_BYTES: usize = 25 * 1024 * 1024;

impl SynthesizedAudio {
    /// File extension of [`Self::to_file_bytes`].
    pub fn extension(&self) -> &'static str {
        match self.format {
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav | AudioFormat::Pcm16 { .. } => "wav",
        }
    }

    /// Bytes of a playable file: raw PCM gets a WAV header; empty, odd or
    /// oversized audio is an error.
    pub fn to_file_bytes(&self) -> Result<Vec<u8>, String> {
        if self.bytes.is_empty() {
            return Err("the provider returned no audio".into());
        }
        if self.bytes.len() > MAX_SPOKEN_BYTES {
            return Err(format!(
                "the provider returned {} bytes of audio, more than the {} byte limit",
                self.bytes.len(),
                MAX_SPOKEN_BYTES
            ));
        }
        match self.format {
            AudioFormat::Wav | AudioFormat::Mp3 => Ok(self.bytes.clone()),
            AudioFormat::Pcm16 { sample_rate } => wav_from_pcm16_bytes(&self.bytes, sample_rate),
        }
    }
}

/// Text-to-speech for one reply.
#[async_trait]
pub trait TextToSpeech: Send + Sync {
    fn provider(&self) -> &str;
    async fn synthesize(&self, text: &str) -> Result<SynthesizedAudio, SpeechError>;
}

/// Which provider produced a result, and the one it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechUsed {
    pub provider: String,
    pub switched_from: Option<String>,
}

/// The configured STT provider and its optional alternate.
#[derive(Clone)]
pub struct SttStack {
    primary: Arc<dyn SpeechToText>,
    alternate: Option<Arc<dyn SpeechToText>>,
}

impl SttStack {
    pub fn new(primary: Arc<dyn SpeechToText>) -> Self {
        Self {
            primary,
            alternate: None,
        }
    }

    /// Used only when the primary reports credit exhaustion.
    pub fn with_alternate(mut self, alternate: Arc<dyn SpeechToText>) -> Self {
        self.alternate = Some(alternate);
        self
    }

    pub fn primary_provider(&self) -> &str {
        self.primary.provider()
    }

    /// The credit-exhaustion fallback, if one is configured.
    pub fn alternate_provider(&self) -> Option<&str> {
        self.alternate.as_ref().map(|a| a.provider())
    }

    /// Whether the primary provider can run on this host.
    pub fn readiness(&self) -> Result<(), String> {
        self.primary.readiness()
    }

    /// Transcribe with the sidecar's fallback rule. On an error after a
    /// switch, `switched_from` is in the result's error message context via
    /// [`FallbackFailure`].
    pub async fn transcribe(&self, wav: &Path) -> Result<(String, SpeechUsed), FallbackFailure> {
        match self.primary.transcribe(wav).await {
            Ok(text) => Ok((
                text,
                SpeechUsed {
                    provider: self.primary.provider().to_string(),
                    switched_from: None,
                },
            )),
            Err(e) => match &self.alternate {
                Some(alt) if e.exhausted() => {
                    tracing::warn!(from = %e.provider, to = alt.provider(), "STT credit exhausted; switching provider");
                    alt.transcribe(wav)
                        .await
                        .map(|text| {
                            (
                                text,
                                SpeechUsed {
                                    provider: alt.provider().to_string(),
                                    switched_from: Some(e.provider.clone()),
                                },
                            )
                        })
                        .map_err(|last| FallbackFailure {
                            error: last,
                            switched_from: Some(e.provider.clone()),
                        })
                }
                _ => Err(FallbackFailure {
                    error: e,
                    switched_from: None,
                }),
            },
        }
    }
}

/// The configured TTS provider and its optional alternate.
#[derive(Clone)]
pub struct TtsStack {
    primary: Arc<dyn TextToSpeech>,
    alternate: Option<Arc<dyn TextToSpeech>>,
}

impl TtsStack {
    pub fn new(primary: Arc<dyn TextToSpeech>) -> Self {
        Self {
            primary,
            alternate: None,
        }
    }

    pub fn with_alternate(mut self, alternate: Arc<dyn TextToSpeech>) -> Self {
        self.alternate = Some(alternate);
        self
    }

    pub fn primary_provider(&self) -> &str {
        self.primary.provider()
    }

    /// The credit-exhaustion fallback, if one is configured.
    pub fn alternate_provider(&self) -> Option<&str> {
        self.alternate.as_ref().map(|a| a.provider())
    }

    /// Synthesise with the sidecar's fallback rule.
    pub async fn synthesize(
        &self,
        text: &str,
    ) -> Result<(SynthesizedAudio, SpeechUsed), FallbackFailure> {
        match self.primary.synthesize(text).await {
            Ok(audio) => Ok((
                audio,
                SpeechUsed {
                    provider: self.primary.provider().to_string(),
                    switched_from: None,
                },
            )),
            Err(e) => match &self.alternate {
                Some(alt) if e.exhausted() => {
                    tracing::warn!(from = %e.provider, to = alt.provider(), "TTS credit exhausted; switching provider");
                    alt.synthesize(text)
                        .await
                        .map(|audio| {
                            (
                                audio,
                                SpeechUsed {
                                    provider: alt.provider().to_string(),
                                    switched_from: Some(e.provider.clone()),
                                },
                            )
                        })
                        .map_err(|last| FallbackFailure {
                            error: last,
                            switched_from: Some(e.provider.clone()),
                        })
                }
                _ => Err(FallbackFailure {
                    error: e,
                    switched_from: None,
                }),
            },
        }
    }
}

impl std::fmt::Debug for SttStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttStack")
            .field("primary", &self.primary.provider())
            .field("alternate", &self.alternate.as_ref().map(|a| a.provider()))
            .finish()
    }
}

impl std::fmt::Debug for TtsStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtsStack")
            .field("primary", &self.primary.provider())
            .field("alternate", &self.alternate_provider())
            .finish()
    }
}

/// The last provider error, and the provider switched away from, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackFailure {
    pub error: SpeechError,
    pub switched_from: Option<String>,
}

impl FallbackFailure {
    /// `deepgram failed (HTTP 500)` / `elevenlabs failed (quota_exceeded)
    /// after switching from deepgram`.
    pub fn owner_summary(&self) -> String {
        if self.error.code == NOT_CONFIGURED {
            return format!(
                "{} is not set up: {}",
                self.error.provider, self.error.message
            );
        }
        let code = &self.error.code;
        let code = if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) {
            format!("HTTP {code}")
        } else {
            code.clone()
        };
        let mut s = format!("{} failed ({code})", self.error.provider);
        if let Some(from) = &self.switched_from {
            s.push_str(&format!(" after switching from {from}"));
        }
        s
    }
}

/// A speech-to-text provider that is not set up on this host (for example
/// whisper.cpp not built): every clip is refused before download with
/// `reason`, and nothing is ever transcribed.
pub struct UnconfiguredStt {
    name: String,
    reason: String,
}

impl UnconfiguredStt {
    pub fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl SpeechToText for UnconfiguredStt {
    fn provider(&self) -> &str {
        &self.name
    }

    fn readiness(&self) -> Result<(), String> {
        Err(self.reason.clone())
    }

    async fn transcribe(&self, _wav: &Path) -> Result<String, SpeechError> {
        Err(SpeechError::new(
            self.name.clone(),
            SpeechOperation::Stt,
            NOT_CONFIGURED,
            self.reason.clone(),
        ))
    }
}

/// Adapter for the existing Rust transcriber
/// (`augmentagent_channel_voice::Transcriber`, whisper.cpp), so Slack clips
/// use the same local speech-to-text path as Telegram voice memos.
pub struct TranscriberStt<T> {
    name: String,
    inner: T,
}

impl<T> TranscriberStt<T> {
    pub fn new(name: impl Into<String>, inner: T) -> Self {
        Self {
            name: name.into(),
            inner,
        }
    }
}

#[async_trait]
impl<T> SpeechToText for TranscriberStt<T>
where
    T: augmentagent_channel_voice::Transcriber,
{
    fn provider(&self) -> &str {
        &self.name
    }

    async fn transcribe(&self, wav: &Path) -> Result<String, SpeechError> {
        use augmentagent_channel_voice::transcribe::TranscribeError as E;
        self.inner.transcribe(wav).await.or_else(|e| {
            let (code, message) = match &e {
                E::Empty => return Ok(String::new()),
                E::Timeout(d) => ("timeout", format!("timed out after {}s", d.as_secs())),
                E::Exit { code, .. } => ("exit", format!("exited with status {code}")),
                E::Io(io) if io.kind() == std::io::ErrorKind::NotFound => (
                    NOT_CONFIGURED,
                    "the whisper.cpp binary or model is not installed (scripts/build-whisper.sh)"
                        .to_string(),
                ),
                E::Io(io) => ("io", io.to_string()),
            };
            Err(SpeechError::new(
                self.name.clone(),
                SpeechOperation::Stt,
                code,
                message,
            ))
        })
    }
}
