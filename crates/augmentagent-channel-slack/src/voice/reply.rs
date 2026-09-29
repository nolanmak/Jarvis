//! Spoken replies (#1297): the answer as an uploaded audio file plus the
//! full text mirror, delivered once through the #1294 outbox.
//!
//! The harness sets [`ReplyMode`] per turn (`Spoken` when the owner asked
//! for a spoken answer, e.g. a "reply by voice" control or a request in
//! the message; the default is `Text`) and calls [`enqueue_spoken_answer`]
//! instead of `delivery::enqueue_answer`:
//!
//! - the text parts keep their keys (`turn:<id>:text:<n>`), so the text
//!   mirror is always delivered, spoken or not;
//! - the audio is the turn's first file (`turn:<id>:file:0`,
//!   [`spoken_audio_key`]), followed by any generated files;
//! - a restart of the same turn finds that key in the outbox and neither
//!   synthesises nor sends again ([`SpeechOutcome::AlreadyQueued`]);
//! - when synthesis fails (after the sidecar's credit-exhaustion fallback)
//!   or no provider is configured, the text is delivered with a one-line
//!   note saying why there is no audio: the turn is never lost.
//!
//! The audio lives under [`default_spoken_reply_root`] (0700 directories,
//! 0600 files) until it is uploaded; [`release_spoken_audio`] removes it
//! once the upload is settled.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use augmentagent_store::delivery::SendStatus;
use augmentagent_store::{Store, StoreError, SurfaceAccountRef, SurfaceConversationRef};
use thiserror::Error;

use super::speech::{SpeechError, SpeechOperation, TtsStack, NOT_CONFIGURED};
use crate::delivery::{
    enqueue_answer, part_idempotency_key, Answer, AnswerEnqueued, AnswerFile, PartKind, PlanError,
    PlanOptions,
};

/// Directory name under the shared state dir.
pub const SLACK_VOICE_REPLIES_DIR: &str = "slack-voice-replies";
/// Name of the uploaded audio (WAV; an MP3 provider gets `.mp3`).
pub const SPOKEN_REPLY_FILENAME: &str = "spoken-reply.wav";
/// Longest text sent to a provider: the sidecar's TTS limit.
pub const MAX_SPEECH_CHARS: usize = 12_000;
/// Upper bound for one synthesis: the sidecar's provider deadline.
pub const TTS_TIMEOUT: Duration = Duration::from_secs(30);

/// How the answer to one turn is delivered. Set per turn by the harness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplyMode {
    /// Text only (the default).
    #[default]
    Text,
    /// Text mirror plus an uploaded audio file.
    Spoken,
}

impl FromStr for ReplyMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" => Ok(ReplyMode::Text),
            "spoken" | "voice" | "audio" => Ok(ReplyMode::Spoken),
            other => Err(format!("unknown reply mode {other:?}: use text or spoken")),
        }
    }
}

impl ReplyMode {
    /// The word stored per conversation.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplyMode::Text => "text",
            ReplyMode::Spoken => "spoken",
        }
    }
}

/// The conversation's own `voice on|off` choice, else (for a thread) its
/// channel's or DM's, else [`ReplyMode::Text`]. Read for every answer, so a
/// change applies to the next one and survives a restart.
pub fn reply_mode_for(
    store: &Store,
    conversation: &SurfaceConversationRef,
) -> Result<ReplyMode, StoreError> {
    Ok(reply_mode_source(store, conversation)?.0)
}

/// [`reply_mode_for`] and whether it was inherited from the parent
/// conversation (a thread's channel or DM).
pub fn reply_mode_source(
    store: &Store,
    conversation: &SurfaceConversationRef,
) -> Result<(ReplyMode, bool), StoreError> {
    let parse = |s: String| s.parse::<ReplyMode>().unwrap_or_default();
    if let Some(own) = store.surface_reply_mode(conversation)? {
        return Ok((parse(own), false));
    }
    if conversation.thread_id().is_some() {
        let parent = SurfaceConversationRef::new(
            conversation.account().clone(),
            conversation.conversation_id(),
            None,
        )
        .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        if let Some(inherited) = store.surface_reply_mode(&parent)? {
            return Ok((parse(inherited), true));
        }
    }
    Ok((ReplyMode::Text, false))
}

/// Store `mode` for exactly `conversation` (`voice on|off`).
pub fn set_reply_mode(
    store: &Store,
    conversation: &SurfaceConversationRef,
    mode: ReplyMode,
    now_ms: i64,
) -> Result<(), StoreError> {
    store.set_surface_reply_mode(conversation, Some(mode.as_str()), now_ms)
}

/// `<state dir>/slack-voice-replies`, `None` without `HOME`.
pub fn default_spoken_reply_root() -> Option<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir().map(|d| d.join(SLACK_VOICE_REPLIES_DIR))
}

/// Outbox key of a turn's spoken audio.
pub fn spoken_audio_key(turn_id: &str) -> String {
    part_idempotency_key(turn_id, PartKind::File, 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpokenReplyOptions {
    /// Parent of the per-turn audio directories; created 0700.
    pub root: PathBuf,
    pub max_speech_chars: usize,
    pub tts_timeout: Duration,
    pub plan: PlanOptions,
}

impl SpokenReplyOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_speech_chars: MAX_SPEECH_CHARS,
            tts_timeout: TTS_TIMEOUT,
            plan: PlanOptions::default(),
        }
    }
}

/// One turn's answer and how to deliver it.
#[derive(Debug, Clone, Copy)]
pub struct SpokenAnswer<'a> {
    pub turn_id: &'a str,
    pub markdown: &'a str,
    /// Generated files, uploaded after the audio.
    pub files: &'a [AnswerFile],
    pub mode: ReplyMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechOutcome {
    /// `ReplyMode::Text`.
    NotRequested,
    /// The audio for this turn is already in the outbox; nothing new.
    AlreadyQueued,
    Synthesized {
        provider: String,
        switched_from: Option<String>,
        bytes: usize,
    },
    /// No audio; the text carries a note saying why.
    Failed(SpeechError),
}

#[derive(Debug)]
pub struct SpokenEnqueued {
    pub enqueued: AnswerEnqueued,
    pub speech: SpeechOutcome,
    /// The stored audio, when there is one on disk.
    pub audio_path: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SpokenReplyError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("spoken reply storage: {0}")]
    Storage(String),
}

/// Stable 64-bit FNV-1a, so a turn's directory survives restarts and
/// upgrades.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// `<root>/<safe prefix>-<hash>` for a turn.
pub fn turn_audio_dir(root: &Path, turn_id: &str) -> PathBuf {
    let prefix: String = turn_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    root.join(format!("{prefix}-{:016x}", fnv1a(turn_id)))
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Write `bytes` to `path` (0600) via a temporary file and a rename, so a
/// crash never leaves a half-written file under the final name.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn spoken_file(path: PathBuf) -> AnswerFile {
    AnswerFile {
        path,
        filename: None,
        title: Some("Spoken reply".into()),
        alt_text: None,
    }
}

/// Plan and enqueue one turn's answer, spoken when `answer.mode` asks for
/// it. Idempotent per turn: see the module docs.
pub async fn enqueue_spoken_answer(
    store: &Store,
    conversation: &SurfaceConversationRef,
    answer: &SpokenAnswer<'_>,
    tts: Option<&TtsStack>,
    opts: &SpokenReplyOptions,
    now_ms: i64,
) -> Result<SpokenEnqueued, SpokenReplyError> {
    let plain = |markdown: &str, files: &[AnswerFile]| {
        enqueue_answer(
            store,
            conversation,
            &Answer {
                turn_id: answer.turn_id,
                markdown,
                files,
            },
            &opts.plan,
            now_ms,
        )
    };
    if answer.mode == ReplyMode::Text {
        return Ok(SpokenEnqueued {
            enqueued: plain(answer.markdown, answer.files)?,
            speech: SpeechOutcome::NotRequested,
            audio_path: None,
        });
    }

    let dir = turn_audio_dir(&opts.root, answer.turn_id);
    let key = spoken_audio_key(answer.turn_id);
    let queued = store
        .outbound_sends_with_key_prefix(conversation.account(), &key, &[])?
        .into_iter()
        .any(|s| s.idempotency_key == key);
    if queued {
        let path = std::fs::read_dir(&dir)
            .ok()
            .and_then(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .find(|p| p.file_stem().is_some_and(|s| s == "spoken-reply"))
            })
            .unwrap_or_else(|| dir.join(SPOKEN_REPLY_FILENAME));
        let exists = path.exists();
        let mut files = vec![spoken_file(path.clone())];
        files.extend_from_slice(answer.files);
        return Ok(SpokenEnqueued {
            enqueued: plain(answer.markdown, &files)?,
            speech: SpeechOutcome::AlreadyQueued,
            audio_path: exists.then_some(path),
        });
    }

    let failed = |error: SpeechError,
                  summary: String|
     -> Result<SpokenEnqueued, SpokenReplyError> {
        tracing::warn!(turn = answer.turn_id, %error, "spoken reply not synthesised; sending text only");
        let markdown = format!(
            "{}\n\n_Spoken reply unavailable: {summary}. The text above is the full answer._",
            answer.markdown.trim_end()
        );
        Ok(SpokenEnqueued {
            enqueued: plain(&markdown, answer.files)?,
            speech: SpeechOutcome::Failed(error),
            audio_path: None,
        })
    };

    let Some(stack) = tts else {
        let e = SpeechError::new(
            "none",
            SpeechOperation::Tts,
            NOT_CONFIGURED,
            "no text-to-speech provider is configured",
        );
        let summary = e.message.clone();
        return failed(e, summary);
    };
    let speech = speakable_text(answer.markdown, opts.max_speech_chars);
    if speech.is_empty() {
        let e = SpeechError::new(
            stack.primary_provider(),
            SpeechOperation::Tts,
            "no_text",
            "the answer has nothing to read aloud",
        );
        let summary = e.message.clone();
        return failed(e, summary);
    }
    let (audio, used) =
        match tokio::time::timeout(opts.tts_timeout, stack.synthesize(&speech)).await {
            Err(_) => {
                let e = SpeechError::new(
                    stack.primary_provider(),
                    SpeechOperation::Tts,
                    "timeout",
                    format!("timed out after {}s", opts.tts_timeout.as_secs_f32()),
                );
                let summary = format!("{} timed out", e.provider);
                return failed(e, summary);
            }
            Ok(Err(failure)) => {
                let summary = failure.owner_summary();
                return failed(failure.error, summary);
            }
            Ok(Ok(ok)) => ok,
        };
    let bytes = match audio.to_file_bytes() {
        Ok(b) => b,
        Err(why) => {
            let e = SpeechError::new(&used.provider, SpeechOperation::Tts, "bad_audio", why);
            let summary = format!("{} returned unusable audio", used.provider);
            return failed(e, summary);
        }
    };
    let path = dir.join(format!("spoken-reply.{}", audio.extension()));
    private_dir(&opts.root)
        .and_then(|_| private_dir(&dir))
        .and_then(|_| write_private_atomic(&path, &bytes))
        .map_err(|e| SpokenReplyError::Storage(format!("{}: {e}", path.display())))?;

    let mut files = vec![spoken_file(path.clone())];
    files.extend_from_slice(answer.files);
    Ok(SpokenEnqueued {
        enqueued: plain(answer.markdown, &files)?,
        speech: SpeechOutcome::Synthesized {
            provider: used.provider,
            switched_from: used.switched_from,
            bytes: bytes.len(),
        },
        audio_path: Some(path),
    })
}

/// Remove a turn's stored audio once its upload is settled (sent, dead
/// letter or abandoned). `Ok(false)` while it is still pending.
pub fn release_spoken_audio(
    store: &Store,
    account: &SurfaceAccountRef,
    turn_id: &str,
    root: &Path,
) -> std::io::Result<bool> {
    let key = spoken_audio_key(turn_id);
    let row = store
        .outbound_sends_with_key_prefix(account, &key, &[])
        .map_err(std::io::Error::other)?
        .into_iter()
        .find(|s| s.idempotency_key == key);
    let settled = row.is_some_and(|r| {
        matches!(
            r.status,
            SendStatus::Sent | SendStatus::DeadLetter | SendStatus::Abandoned
        )
    });
    if !settled {
        return Ok(false);
    }
    match std::fs::remove_dir_all(turn_audio_dir(root, turn_id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

/// Replace `[text](url)` with `text` and drop bare URLs.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == ']') {
                let close = i + 1 + close;
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = chars[close + 2..].iter().position(|&c| c == ')') {
                        out.extend(&chars[i + 1..close]);
                        i = close + 2 + end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out.split_whitespace()
        .filter(|w| !(w.starts_with("http://") || w.starts_with("https://")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The answer as prose for a speech provider: Markdown markers, code
/// blocks and URLs removed, list items read as sentences, cut at
/// `max_chars` with a pointer to the text reply.
pub fn speakable_text(markdown: &str, max_chars: usize) -> String {
    const CODE_NOTE: &str = "Code omitted; the code is in the text reply.";
    const MORE: &str = "The rest is in the text reply.";
    let mut parts: Vec<String> = Vec::new();
    let mut in_fence = false;
    for raw in markdown.lines() {
        let line = raw.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            if !in_fence && parts.last().map(String::as_str) != Some(CODE_NOTE) {
                parts.push(CODE_NOTE.into());
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() {
            continue;
        }
        let mut body = line.trim_start_matches('#').trim_start();
        let mut list_item = false;
        for bullet in ["- ", "* ", "+ ", "> "] {
            if let Some(rest) = body.strip_prefix(bullet) {
                body = rest;
                list_item = bullet != "> ";
                break;
            }
        }
        let mut text = strip_links(body);
        text.retain(|c| !matches!(c, '*' | '`' | '#' | '~' | '|'));
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let ends = text.ends_with(['.', '!', '?', ':', ';', ',']);
        parts.push(if list_item && !ends {
            format!("{text}.")
        } else {
            text
        });
    }
    let mut s = parts.join(" ");
    if s.chars().count() > max_chars {
        let budget = max_chars.saturating_sub(MORE.len() + 2);
        let cut: String = s.chars().take(budget).collect();
        let cut = match cut.rfind(char::is_whitespace) {
            Some(at) if at > 0 => cut[..at].to_string(),
            _ => cut,
        };
        s = format!("{}… {MORE}", cut.trim_end());
    }
    s
}
