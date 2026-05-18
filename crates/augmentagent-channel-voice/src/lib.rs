//! Voice memo capture channel. A single private Telegram bot accepts voice
//! memos from a hard chat-id allowlist, transcribes them via local
//! whisper.cpp, structures the transcript with a Haiku call, and feeds the
//! result into the EXISTING `spawn_ingest` pipeline as a
//! `DecisionKind::Capture` / `IngestTrigger::VoiceMemo`. No ingest changes.
//! See issue #80.

pub mod channel;
pub mod confirm;
pub mod extract;
pub mod listener;
pub mod telegram;
pub mod transcribe;

/// Platform discriminator used in `Email::platform` rows for transcribed
/// voice captures.
pub const PLATFORM: &str = "voice";

/// `account_entity_id` prefix applied to stored rows so multi-device captures
/// stay distinguishable.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "voice";

pub use channel::{ingest_memo, synthetic_memo_email};
pub use extract::MemoRecord;
pub use listener::{
    default_allowlist_path, load_allowlist, load_token, VoiceListener, BOT_KEY,
    KEYRING_PLATFORM,
};
pub use telegram::VoiceTelegramClient;
pub use transcribe::{Transcriber, WhisperCppTranscriber};
