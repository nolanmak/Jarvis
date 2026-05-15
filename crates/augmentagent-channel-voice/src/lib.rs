//! Voice memo capture channel. Watches a configured drop directory (or
//! Whisper-transcribed audio queue) and feeds each transcript into the wiki
//! ingest pipeline as a `Capture` decision. Emits `IngestTrigger::VoiceMemo`.

pub mod capture;
pub mod channel;
pub mod transcribe;

/// Platform discriminator used in `Email::platform` rows for transcribed
/// voice captures.
pub const PLATFORM: &str = "voice";

/// `account_entity_id` prefix applied to stored rows so multi-device captures
/// stay distinguishable.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "voice";
