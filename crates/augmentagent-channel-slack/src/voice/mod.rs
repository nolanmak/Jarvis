//! #1297 — voice clips in, spoken replies out, in the same Slack
//! conversation as typed turns. Asynchronous clips only: live voice is a
//! separate, open blocker (`docs/SLACK-LIVE-VOICE.md`, #1298) and this
//! module does not satisfy it.
//!
//! **Inbound.** [`crate::inbound::prepare_inbound_with_voice`] runs the
//! #1293 pipeline with a [`VoiceInbound`]: audio and video clips (Slack
//! clips and uploaded audio, see [`audio::clip_format`]) are downloaded with
//! the same bot-token client, private per-message directory and streamed
//! size cap, decoded to 16 kHz mono WAV by `ffmpeg` ([`audio`]), transcribed
//! by the [`SttStack`] (sidecar fallback rule, [`speech`]) and removed. The
//! transcript becomes turn text ([`crate::inbound::InboundMessage::turn_text`])
//! and a line shown back to the owner
//! ([`crate::inbound::InboundMessage::transcript_notice`]). Unsupported
//! formats, over-long or oversized clips, decode failures and provider
//! failures are owner-facing "skipped" reasons; nothing is lost silently.
//!
//! Owner-only: the harness calls the pipeline for input that
//! `owner::admit` dispatched, never before; non-owner audio is rejected at
//! the gate and is never downloaded or transcribed.
//!
//! **Outbound.** [`reply::enqueue_spoken_answer`] takes the per-turn
//! [`reply::ReplyMode`]; for `Spoken` it synthesises the answer with the
//! [`TtsStack`] and enqueues the audio file after the full text mirror via
//! `delivery::enqueue_answer`, so both are delivered once under the turn's
//! idempotency keys.

pub mod audio;
pub mod fake;
pub mod providers;
pub mod reply;
pub mod speech;

use std::path::{Path, PathBuf};
use std::time::Duration;

use augmentagent_docs::ConvertOptions;
use tokio_util::sync::CancellationToken;

pub use speech::{
    FallbackFailure, SpeechError, SpeechToText, SpeechUsed, SttStack, TextToSpeech, TtsStack,
};

use audio::{decode_for_stt, format_duration, wav_duration_ms, ClipMedia};

/// Largest clip downloaded (declared and streamed).
pub const MAX_CLIP_BYTES: u64 = 25 * 1_048_576;
/// Longest clip transcribed.
pub const MAX_CLIP_DURATION: Duration = Duration::from_secs(10 * 60);
/// Clips transcribed per message; the rest are skipped with a reason.
pub const MAX_CLIPS_PER_MESSAGE: usize = 3;
/// Upper bound for one `ffmpeg` decode.
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(60);
/// Upper bound for one transcription (whisper.cpp has its own 60 s cap).
pub const STT_TIMEOUT: Duration = Duration::from_secs(120);

/// Bounds on voice input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipLimits {
    pub max_bytes: u64,
    pub max_duration: Duration,
    pub max_clips: usize,
    pub stt_timeout: Duration,
}

impl Default for ClipLimits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_CLIP_BYTES,
            max_duration: MAX_CLIP_DURATION,
            max_clips: MAX_CLIPS_PER_MESSAGE,
            stt_timeout: STT_TIMEOUT,
        }
    }
}

/// The daemon's speech setup for the live surface (`serve`): clips from
/// the owner are transcribed with `stt`, and answers in a conversation whose
/// reply mode is spoken (`voice on`) are synthesised with `tts`.
#[derive(Clone, Debug)]
pub struct SlackVoice {
    /// Always present: an unconfigured provider
    /// ([`speech::UnconfiguredStt`]) makes every clip an owner-facing
    /// "can't be transcribed" notice instead of a silent "unsupported".
    pub stt: SttStack,
    /// `None`: spoken replies arrive as text with a note saying why.
    pub tts: Option<TtsStack>,
    /// `ffmpeg` lookup and timeout.
    pub tools: ConvertOptions,
    pub limits: ClipLimits,
    /// Where synthesised audio waits for its upload.
    pub replies: reply::SpokenReplyOptions,
}

/// What `voice status` reports: the provider in use, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceReadiness {
    pub stt: Result<String, String>,
    pub tts: Result<String, String>,
}

fn describe(primary: &str, alternate: Option<&str>) -> String {
    match alternate {
        Some(alt) => format!("{primary} (then {alt} if {primary} runs out of credit)"),
        None => primary.to_string(),
    }
}

impl SlackVoice {
    /// Default `ffmpeg` lookup and limits.
    pub fn new(stt: SttStack, tts: Option<TtsStack>, replies_root: PathBuf) -> Self {
        Self {
            stt,
            tts,
            tools: ConvertOptions {
                timeout: DECODE_TIMEOUT,
                ..ConvertOptions::default()
            },
            limits: ClipLimits::default(),
            replies: reply::SpokenReplyOptions::new(replies_root),
        }
    }

    /// The pipeline's view of this setup.
    pub fn inbound(&self) -> VoiceInbound<'_> {
        VoiceInbound {
            stt: &self.stt,
            tools: self.tools.clone(),
            limits: self.limits.clone(),
        }
    }

    /// Readiness from the stacks themselves (a missing TTS stack gives a
    /// generic reason; `serve` reports the configuration's own).
    pub fn readiness(&self) -> VoiceReadiness {
        VoiceReadiness {
            stt: self
                .stt
                .readiness()
                .map(|()| describe(self.stt.primary_provider(), self.stt.alternate_provider())),
            tts: match &self.tts {
                Some(t) => Ok(describe(t.primary_provider(), t.alternate_provider())),
                None => Err("no text-to-speech provider is configured".into()),
            },
        }
    }
}

/// Speech-to-text for the inbound pipeline.
#[derive(Clone)]
pub struct VoiceInbound<'a> {
    pub stt: &'a SttStack,
    /// `ffmpeg` lookup (PATH, then the service fallbacks) and its timeout.
    pub tools: ConvertOptions,
    pub limits: ClipLimits,
}

impl<'a> VoiceInbound<'a> {
    /// Default lookup, [`DECODE_TIMEOUT`] and [`ClipLimits::default`].
    pub fn new(stt: &'a SttStack) -> Self {
        Self {
            stt,
            tools: ConvertOptions {
                timeout: DECODE_TIMEOUT,
                ..ConvertOptions::default()
            },
            limits: ClipLimits::default(),
        }
    }
}

/// One transcribed clip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipTranscript {
    pub file_id: String,
    /// As the owner sent it (display only).
    pub original_name: String,
    pub media: ClipMedia,
    pub text: String,
    /// Decoded length.
    pub duration_ms: u64,
    pub provider: String,
    /// The provider that ran out of credit, when the alternate was used.
    pub switched_from: Option<String>,
}

impl ClipTranscript {
    /// The owner-facing transcript line.
    pub fn notice_line(&self) -> String {
        let mut via = String::new();
        if let Some(from) = &self.switched_from {
            via = format!(", via {} because {from} ran out of credit", self.provider);
        }
        format!(
            "\u{1F399}\u{FE0F} Transcript of {} ({}{via}): {}",
            self.original_name,
            format_duration(self.duration_ms),
            self.text
        )
    }
}

/// Outcome of [`transcribe_clip`].
#[derive(Debug)]
pub(crate) enum ClipOutcome {
    Transcript(ClipTranscript),
    /// Owner-facing reason.
    Rejected(String),
    Cancelled,
}

/// Removes the listed files on drop (success, failure or cancellation).
struct RemoveOnDrop(Vec<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Decode and transcribe one downloaded clip, then delete it.
pub(crate) async fn transcribe_clip(
    voice: &VoiceInbound<'_>,
    file_id: &str,
    name: &str,
    media: ClipMedia,
    downloaded: &Path,
    cancel: &CancellationToken,
) -> ClipOutcome {
    let wav = downloaded.with_extension("stt.wav");
    let _cleanup = RemoveOnDrop(vec![downloaded.to_path_buf(), wav.clone()]);
    let limit = voice.limits.max_duration;

    let decoded = tokio::select! {
        _ = cancel.cancelled() => return ClipOutcome::Cancelled,
        r = decode_for_stt(downloaded, &wav, limit, &voice.tools) => r,
    };
    if let Err(e) = decoded {
        tracing::warn!(file_id, "slack voice clip decode failed: {e}");
        return ClipOutcome::Rejected(format!("couldn't decode the voice clip: {e}"));
    }
    let _ = std::fs::remove_file(downloaded);
    let bytes = match std::fs::read(&wav) {
        Ok(b) => b,
        Err(e) => return ClipOutcome::Rejected(format!("couldn't read the decoded audio ({e})")),
    };
    let Some((duration_ms, _rate)) = wav_duration_ms(&bytes) else {
        return ClipOutcome::Rejected("couldn't decode the voice clip: no audio".into());
    };
    if duration_ms > limit.as_millis() as u64 {
        return ClipOutcome::Rejected(format!(
            "voice clip longer than {}",
            format_duration(limit.as_millis() as u64)
        ));
    }
    drop(bytes);

    let result = tokio::select! {
        _ = cancel.cancelled() => return ClipOutcome::Cancelled,
        r = tokio::time::timeout(voice.limits.stt_timeout, voice.stt.transcribe(&wav)) => r,
    };
    let (text, used) = match result {
        Err(_) => {
            return ClipOutcome::Rejected(format!(
                "couldn't transcribe: {} timed out after {}s; the clip was not run, resend it or type the message",
                voice.stt.primary_provider(),
                voice.limits.stt_timeout.as_secs_f32()
            ))
        }
        Ok(Err(failure)) => {
            tracing::warn!(file_id, error = %failure.error, "slack voice clip transcription failed");
            return ClipOutcome::Rejected(format!(
                "couldn't transcribe: {}; the clip was not run, resend it or type the message",
                failure.owner_summary()
            ));
        }
        Ok(Ok(ok)) => ok,
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return ClipOutcome::Rejected("no speech recognised in the voice clip".into());
    }
    ClipOutcome::Transcript(ClipTranscript {
        file_id: file_id.to_string(),
        original_name: name.to_string(),
        media,
        text,
        duration_ms,
        provider: used.provider,
        switched_from: used.switched_from,
    })
}
