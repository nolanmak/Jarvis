//! #1297 — `augmentagent slack voice`: operator/debug paths for Slack voice
//! clips and spoken replies, through the same library calls the turn
//! harness (#1288) will make.
//!
//! - `transcribe --channel C… --ts TS` reads one message, runs its files
//!   through `inbound::prepare_inbound_with_voice` (download with the
//!   installed app's bot token, `ffmpeg` decode, speech-to-text) and prints
//!   the transcript line, the turn text and anything skipped. Files are
//!   removed before the command exits.
//! - `speak --channel C… [--thread TS] (--text-file F | --stdin)` delivers
//!   an answer as a spoken reply: text mirror plus uploaded audio through
//!   the outbox (`voice::reply::enqueue_spoken_answer`). Re-running the same
//!   command synthesises and sends nothing new.
//!
//! Speech providers: speech-to-text is the existing whisper.cpp transcriber
//! (`vendor/whisper`, as for Telegram voice memos). No text-to-speech
//! provider has a Rust file adapter yet (Deepgram/ElevenLabs TTS runs only
//! inside the Discord voice sidecar), so `speak` fails clearly in release
//! builds. For local QA, a debug build honours [`TEST_SPEECH_ENV`], which
//! swaps in the offline scripted fakes; release builds ignore it.
//!
//! Tokens are never printed or logged.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use augmentagent_channel_slack::app::{api_base_from, SlackAppStore, SLACK_API_BASE_ENV};
use augmentagent_channel_slack::delivery::{DispatchOutcome, SlackOutboxDispatcher};
use augmentagent_channel_slack::inbound::{
    default_inbound_root, prepare_inbound_with_voice, InboundOptions,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::file_refs;
use augmentagent_channel_slack::transport::web::{
    test_file_hosts_from, DownloadLimits, HistoryQuery, SlackWebApi, SLACK_TEST_FILE_HOSTS_ENV,
};
use augmentagent_channel_slack::transport::{HttpSlackWebApi, WebApiConfig};
use augmentagent_channel_slack::voice::audio::resolve_ffmpeg;
use augmentagent_channel_slack::voice::fake::{ScriptedStt, ScriptedTts, SttScript, TtsScript};
use augmentagent_channel_slack::voice::reply::{
    default_spoken_reply_root, enqueue_spoken_answer, release_spoken_audio, ReplyMode,
    SpeechOutcome, SpokenAnswer, SpokenReplyOptions,
};
use augmentagent_channel_slack::voice::speech::TranscriberStt;
use augmentagent_channel_slack::voice::{SttStack, TtsStack, VoiceInbound};
use augmentagent_docs::inbound::RejectReason;
use augmentagent_store::Store;
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// Debug builds only: replace the speech providers with offline fakes.
/// `ok:TEXT` transcribes every clip as TEXT and speaks a short tone;
/// `fail:CODE` makes the provider fail with CODE (no vendor switch unless
/// CODE is `402`/`quota_exceeded`); `exhausted:TEXT` exhausts the primary
/// (deepgram) and succeeds on the alternate (elevenlabs); `exhausted-all`
/// exhausts both. Ignored in release builds.
pub const TEST_SPEECH_ENV: &str = "AUGMENTAGENT_TEST_SLACK_SPEECH";

/// Largest answer read from a file or stdin.
const MAX_TEXT_BYTES: u64 = 1024 * 1024;
/// Pages of a thread read while looking for `--ts`.
const MAX_PAGES: usize = 5;

#[derive(Subcommand, Debug, Clone)]
pub enum SlackVoiceOp {
    /// Transcribe one message's voice clips through the inbound pipeline
    /// and print the transcript and turn text (files are removed after).
    Transcribe(TranscribeArgs),
    /// Deliver an answer as a spoken reply: text mirror plus uploaded
    /// audio, through the outbox. Re-running sends nothing twice.
    Speak(SpeakArgs),
}

#[derive(Args, Debug, Clone)]
pub struct TranscribeArgs {
    /// Channel, group or DM ID the message is in.
    #[arg(long)]
    pub channel: String,
    /// The message `ts`; a thread reply works too.
    #[arg(long)]
    pub ts: String,
    /// Workspace team id; defaults to the only installed workspace.
    #[arg(long)]
    pub team: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct SpeakArgs {
    /// Channel, group or DM ID to deliver into.
    #[arg(long)]
    pub channel: String,
    /// Parent message `ts`: reply in that thread.
    #[arg(long)]
    pub thread: Option<String>,
    /// Markdown answer to speak and mirror as text.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "stdin",
        required_unless_present = "stdin"
    )]
    pub text_file: Option<PathBuf>,
    /// Read the answer from stdin.
    #[arg(long)]
    pub stdin: bool,
    /// Workspace team id; defaults to the only installed workspace.
    #[arg(long)]
    pub team: Option<String>,
    /// Turn ID the outbox keys derive from. Default: a hash of the
    /// workspace, conversation and text, so a re-run sends nothing new.
    #[arg(long)]
    pub turn_id: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
    pub json: bool,
}

struct Failure {
    code: &'static str,
    message: String,
    recovery: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>, recovery: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: recovery.into(),
        }
    }
}

fn fail(json_out: bool, f: &Failure) -> ! {
    if json_out {
        println!(
            "{}",
            json!({"ok": false, "error": f.code, "message": f.message, "recovery": f.recovery})
        );
    } else {
        eprintln!("error: {}", f.message);
        eprintln!("recovery: {}", f.recovery);
    }
    std::process::exit(1);
}

/// Speech providers for this command.
struct Providers {
    stt: SttStack,
    tts: Option<TtsStack>,
    /// `true` when the debug-only fakes are in use.
    fake: bool,
}

/// The [`TEST_SPEECH_ENV`] fakes, in a debug build only.
fn test_providers(value: Option<&str>) -> Option<Result<Providers, String>> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let value = value.map(str::trim).filter(|v| !v.is_empty())?;
    let (mode, arg) = value.split_once(':').unwrap_or((value, ""));
    let text = if arg.is_empty() {
        "synthetic transcript"
    } else {
        arg
    };
    let stt_ok = |name: &str| ScriptedStt::new(name, vec![SttScript::Text(text.into())]);
    let tts_ok = |name: &str| ScriptedTts::new(name, vec![TtsScript::Wav(1_500)]);
    let (stt, tts) = match mode {
        "ok" => (
            SttStack::new(Arc::new(stt_ok("deepgram"))),
            TtsStack::new(Arc::new(tts_ok("deepgram"))),
        ),
        "fail" => {
            let code = if arg.is_empty() { "500" } else { arg };
            (
                SttStack::new(Arc::new(ScriptedStt::new(
                    "deepgram",
                    vec![SttScript::Fail(code.into())],
                )))
                .with_alternate(Arc::new(stt_ok("elevenlabs"))),
                TtsStack::new(Arc::new(ScriptedTts::new(
                    "deepgram",
                    vec![TtsScript::Fail(code.into())],
                )))
                .with_alternate(Arc::new(tts_ok("elevenlabs"))),
            )
        }
        "exhausted" => (
            SttStack::new(Arc::new(ScriptedStt::new(
                "deepgram",
                vec![SttScript::Fail("402".into())],
            )))
            .with_alternate(Arc::new(stt_ok("elevenlabs"))),
            TtsStack::new(Arc::new(ScriptedTts::new(
                "deepgram",
                vec![TtsScript::Fail("402".into())],
            )))
            .with_alternate(Arc::new(tts_ok("elevenlabs"))),
        ),
        "exhausted-all" => (
            SttStack::new(Arc::new(ScriptedStt::new(
                "deepgram",
                vec![SttScript::Fail("402".into())],
            )))
            .with_alternate(Arc::new(ScriptedStt::new(
                "elevenlabs",
                vec![SttScript::Fail("quota_exceeded".into())],
            ))),
            TtsStack::new(Arc::new(ScriptedTts::new(
                "deepgram",
                vec![TtsScript::Fail("402".into())],
            )))
            .with_alternate(Arc::new(ScriptedTts::new(
                "elevenlabs",
                vec![TtsScript::Fail("quota_exceeded".into())],
            ))),
        ),
        other => {
            return Some(Err(format!(
                "{TEST_SPEECH_ENV}={other:?} is not one of ok, fail, exhausted, exhausted-all"
            )))
        }
    };
    Some(Ok(Providers {
        stt,
        tts: Some(tts),
        fake: true,
    }))
}

fn providers() -> Result<Providers, Failure> {
    let raw = std::env::var(TEST_SPEECH_ENV).ok();
    if let Some(p) = test_providers(raw.as_deref()) {
        tracing::warn!(
            "{TEST_SPEECH_ENV} replaces the speech providers with offline fakes (debug build)"
        );
        return p.map_err(|m| {
            Failure::new("invalid_test_speech", m, format!("unset {TEST_SPEECH_ENV}"))
        });
    }
    // The existing Rust speech-to-text path (Telegram voice memos).
    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let whisper = augmentagent_channel_voice::WhisperCppTranscriber::from_repo_root(&repo_root);
    Ok(Providers {
        stt: SttStack::new(Arc::new(TranscriberStt::new("whisper-cpp", whisper))),
        tts: None,
        fake: false,
    })
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn valid_ts(s: &str) -> bool {
    s.split_once('.').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && (a.chars().chain(b.chars())).all(|c| c.is_ascii_digit())
    })
}

fn app_err(e: augmentagent_channel_slack::app::SlackAppError) -> Failure {
    Failure::new(e.code(), e.to_string(), e.recovery())
}

/// The installed app's Web API client (bot token), honouring the test API
/// base and loopback file host overrides.
fn slack_api(
    team: Option<&str>,
    cancel: &CancellationToken,
) -> Result<(String, HttpSlackWebApi), Failure> {
    let raw_hosts = std::env::var(SLACK_TEST_FILE_HOSTS_ENV).ok();
    let test_hosts = test_file_hosts_from(raw_hosts.as_deref()).map_err(|m| {
        Failure::new(
            "invalid_test_file_host",
            m,
            format!("unset {SLACK_TEST_FILE_HOSTS_ENV}; it only accepts loopback host:port entries for local mock servers"),
        )
    })?;
    if !test_hosts.is_empty() {
        tracing::warn!(hosts = ?test_hosts, "{SLACK_TEST_FILE_HOSTS_ENV} adds loopback test file hosts");
    }
    let creds = SlackAppStore::default_store();
    let team = creds.resolve_team(team).map_err(app_err)?;
    let installed = creds.load(&team).map_err(app_err)?.ok_or_else(|| {
        app_err(
            augmentagent_channel_slack::app::SlackAppError::NotInstalled {
                team_id: team.clone(),
            },
        )
    })?;
    let raw_base = std::env::var(SLACK_API_BASE_ENV).ok();
    let base_url = api_base_from(raw_base.as_deref()).map_err(app_err)?;
    if raw_base.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        tracing::warn!(api_base = %base_url, "{SLACK_API_BASE_ENV} overrides the Slack Web API base URL");
    }
    let mut limits = DownloadLimits::default();
    limits.allowed_hosts.extend(test_hosts);
    let api = HttpSlackWebApi::new(
        installed.bot_token.clone(),
        WebApiConfig {
            base_url,
            ..WebApiConfig::default()
        },
    )
    .map_err(|e| Failure::new("slack_api", e.to_string(), "check network access to Slack"))?
    .with_download_limits(limits)
    .scoped(cancel.clone());
    Ok((installed.team_id, api))
}

/// Entry point for `augmentagent slack voice`.
pub async fn run(op: &SlackVoiceOp, store: &Store) -> Result<()> {
    match op {
        SlackVoiceOp::Transcribe(args) => match transcribe(args).await {
            Ok(v) => {
                let ok = v["ok"] == json!(true);
                if args.json {
                    println!("{v}");
                } else {
                    print_transcribe(&v);
                }
                if !ok {
                    std::process::exit(1);
                }
                Ok(())
            }
            Err(f) => fail(args.json, &f),
        },
        SlackVoiceOp::Speak(args) => match speak(args, store).await {
            Ok(v) => {
                let ok = v["ok"] == json!(true);
                if args.json {
                    println!("{v}");
                } else {
                    print_speak(&v);
                }
                if !ok {
                    std::process::exit(1);
                }
                Ok(())
            }
            Err(f) => fail(args.json, &f),
        },
    }
}

async fn transcribe(args: &TranscribeArgs) -> Result<Value, Failure> {
    if !valid_id(&args.channel) || !valid_ts(&args.ts) {
        return Err(Failure::new(
            "invalid_target",
            "--channel must be a Slack conversation ID and --ts a message ts",
            "e.g. --channel D0123ABCD --ts 1700000000.000100 (\"Copy link\" on the message shows both)",
        ));
    }
    let providers = providers()?;
    let cancel = CancellationToken::new();
    let (_team, api) = slack_api(args.team.as_deref(), &cancel)?;

    let mut cursor = None;
    let mut found = None;
    for _ in 0..MAX_PAGES {
        let page = api
            .conversations_replies(HistoryQuery {
                channel: args.channel.clone(),
                thread_ts: Some(args.ts.clone()),
                oldest: None,
                limit: 200,
                cursor: cursor.clone(),
                include_all_metadata: false,
            })
            .await
            .map_err(|e| {
                Failure::new(
                    "slack_api",
                    format!("could not read the message: {e}"),
                    "check --channel/--ts and that the app is in the conversation (needs the *:history scopes)",
                )
            })?;
        found = page.messages.into_iter().find(|m| m.ts == args.ts);
        if found.is_some() || !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    let message = found.ok_or_else(|| {
        Failure::new(
            "message_not_found",
            format!("no message {} in {}", args.ts, args.channel),
            "copy the message link in Slack and pass its channel and ts",
        )
    })?;
    let files = file_refs(&message.raw);
    let text = message.text.clone().unwrap_or_default();
    let root = default_inbound_root().ok_or_else(|| {
        Failure::new(
            "storage",
            "cannot resolve the state dir (HOME unset)",
            "set HOME",
        )
    })?;
    let opts = InboundOptions::new(root.clone());
    let voice = VoiceInbound::new(&providers.stt);
    let ffmpeg = resolve_ffmpeg(&voice.tools);

    let on_signal = cancel.clone();
    let watcher = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_signal.cancel();
        }
    });
    let prepared =
        prepare_inbound_with_voice(&api, &text, &files, &opts, Some(&voice), &cancel).await;
    watcher.abort();
    let msg = prepared.map_err(|e| {
        Failure::new(
            "inbound",
            e.to_string(),
            "for storage errors, check that the state dir is a private directory you own",
        )
    })?;

    let transcripts: Vec<Value> = msg
        .transcripts
        .iter()
        .map(|t| {
            json!({
                "file_id": t.file_id,
                "name": t.original_name,
                "media": t.media.as_str(),
                "duration_ms": t.duration_ms,
                "provider": t.provider,
                "switched_from": t.switched_from,
                "text": t.text,
            })
        })
        .collect();
    let rejected: Vec<Value> = msg
        .rejected
        .iter()
        .map(|r| {
            let (reason, detail) = match &r.reason {
                RejectReason::Oversize { size, limit } => {
                    ("oversize", format!("{size} bytes > {limit} bytes"))
                }
                RejectReason::SecurityDenylist => ("security", String::new()),
                RejectReason::UnsupportedType { content_type, ext } => (
                    "unsupported",
                    content_type.clone().or(ext.clone()).unwrap_or_default(),
                ),
                RejectReason::Unavailable(why) => ("unavailable", why.clone()),
            };
            json!({"name": r.filename, "reason": reason, "detail": detail})
        })
        .collect();
    let ok = !msg.transcripts.is_empty() && msg.rejected.is_empty();
    let mut v = json!({
        "ok": ok,
        "channel": args.channel,
        "ts": args.ts,
        "files": files.len(),
        "stt_provider": providers.stt.primary_provider(),
        "fake_providers": providers.fake,
        "ffmpeg": ffmpeg.map(|p| p.display().to_string()),
        "transcripts": transcripts,
        "transcript_notice": msg.transcript_notice(),
        "rejected": rejected,
        "notice": msg.rejection_notice(),
        "starts_turn": msg.starts_turn(),
        "turn_text": msg.turn_text(),
        "prompt": msg.prompt,
    });
    v["cleaned_up"] = json!(msg.cleanup().is_ok());
    Ok(v)
}

fn print_transcribe(v: &Value) {
    println!(
        "message {} in {}: {} file(s); speech-to-text: {}{}; ffmpeg: {}",
        v["ts"].as_str().unwrap_or_default(),
        v["channel"].as_str().unwrap_or_default(),
        v["files"],
        v["stt_provider"].as_str().unwrap_or_default(),
        if v["fake_providers"] == json!(true) {
            " (offline test fake)"
        } else {
            ""
        },
        v["ffmpeg"].as_str().unwrap_or("not found"),
    );
    if let Some(line) = v["transcript_notice"].as_str() {
        println!("{line}");
    }
    for r in v["rejected"].as_array().into_iter().flatten() {
        println!(
            "  skipped   {:<11} {}: {}",
            r["reason"].as_str().unwrap_or_default(),
            r["name"].as_str().unwrap_or_default(),
            r["detail"].as_str().unwrap_or_default()
        );
    }
    if let Some(notice) = v["notice"].as_str() {
        println!("owner notice: {notice}");
    }
    println!(
        "starts a turn: {}; files removed: {}",
        v["starts_turn"], v["cleaned_up"]
    );
    println!(
        "--- turn text ---\n{}",
        v["turn_text"].as_str().unwrap_or_default()
    );
}

fn read_text(args: &SpeakArgs) -> Result<String, Failure> {
    let bad = |m: String| {
        Failure::new(
            "text_input",
            m,
            "pass the answer with --text-file PATH or --stdin (UTF-8, at most 1 MiB)",
        )
    };
    let mut buf = String::new();
    if let Some(path) = &args.text_file {
        std::fs::File::open(path)
            .and_then(|f| f.take(MAX_TEXT_BYTES + 1).read_to_string(&mut buf))
            .map_err(|e| bad(format!("read {}: {e}", path.display())))?;
    } else {
        std::io::stdin()
            .lock()
            .take(MAX_TEXT_BYTES + 1)
            .read_to_string(&mut buf)
            .map_err(|e| bad(format!("read stdin: {e}")))?;
    }
    if buf.len() as u64 > MAX_TEXT_BYTES {
        return Err(bad("answer is larger than 1 MiB".into()));
    }
    if buf.trim().is_empty() {
        return Err(bad("the answer is empty".into()));
    }
    Ok(buf)
}

fn default_turn_id(team: &str, channel: &str, thread: Option<&str>, text: &str) -> String {
    let mut h = Sha256::new();
    for part in ["spoken", team, channel, thread.unwrap_or(""), text] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    let digest = h.finalize();
    let hex: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("cli-voice-{hex}")
}

async fn speak(args: &SpeakArgs, store: &Store) -> Result<Value, Failure> {
    let text = read_text(args)?;
    let providers = providers()?;
    let Some(tts) = providers.tts.as_ref() else {
        return Err(Failure::new(
            "tts_unavailable",
            "no text-to-speech provider has a file adapter yet: Deepgram/ElevenLabs TTS runs only inside the Discord voice sidecar",
            "deliver the answer as text with `augmentagent slack deliver`; spoken replies need the TTS adapter tracked in #1297",
        ));
    };
    let cancel = CancellationToken::new();
    let (team, api) = slack_api(args.team.as_deref(), &cancel)?;
    let invalid = |m: String| {
        Failure::new(
            "invalid_target",
            m,
            "--channel is a Slack conversation ID (e.g. C0123ABCD) and --thread a message ts (e.g. 1700000000.000100)",
        )
    };
    let workspace =
        SlackWorkspace::new(&team, None).map_err(|e| invalid(format!("team id: {e}")))?;
    let conversation = workspace
        .conversation(&args.channel, args.thread.as_deref())
        .map_err(|e| invalid(e.to_string()))?;
    let turn_id = args
        .turn_id
        .clone()
        .unwrap_or_else(|| default_turn_id(&team, &args.channel, args.thread.as_deref(), &text));
    let root = default_spoken_reply_root().ok_or_else(|| {
        Failure::new(
            "storage",
            "cannot resolve the state dir (HOME unset)",
            "set HOME",
        )
    })?;
    let opts = SpokenReplyOptions::new(root.clone());
    let answer = SpokenAnswer {
        turn_id: &turn_id,
        markdown: &text,
        files: &[],
        mode: ReplyMode::Spoken,
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let spoken = enqueue_spoken_answer(store, &conversation, &answer, Some(tts), &opts, now_ms)
        .await
        .map_err(|e| {
            Failure::new(
                "plan",
                e.to_string(),
                "check the answer text and the state dir",
            )
        })?;
    let dispatched = SlackOutboxDispatcher::new(store, &api, &workspace)
        .drain(now_ms)
        .await
        .map_err(|e| Failure::new("store", e.to_string(), "check AUGMENTAGENT_DB is writable"))?;

    let mut rows = Vec::new();
    let mut all_sent = true;
    for planned in &spoken.enqueued.sends {
        let row = store
            .outbound_send(planned.id)
            .map_err(|e| Failure::new("store", e.to_string(), "check AUGMENTAGENT_DB"))?
            .ok_or_else(|| Failure::new("store", "outbox row vanished", "run the command again"))?;
        let sent_now = dispatched
            .iter()
            .any(|d| d.id == planned.id && matches!(d.outcome, DispatchOutcome::Sent { .. }));
        all_sent &= row.status.as_str() == "sent";
        rows.push(json!({
            "idempotency_key": row.idempotency_key,
            "operation": row.operation.as_str(),
            "status": row.status.as_str(),
            "sent_now": sent_now,
            "already_enqueued": planned.status.is_some(),
            "provider_message_id": row.provider_message_id,
            "last_error": row.last_error,
        }));
    }
    let released = spoken.audio_path.is_some().then(|| {
        release_spoken_audio(store, &workspace.account(), &turn_id, &root).unwrap_or(false)
    });
    let speech = match &spoken.speech {
        SpeechOutcome::NotRequested => json!({"status": "not_requested"}),
        SpeechOutcome::AlreadyQueued => json!({"status": "already_queued"}),
        SpeechOutcome::Synthesized {
            provider,
            switched_from,
            bytes,
        } => {
            json!({"status": "synthesized", "provider": provider, "switched_from": switched_from, "bytes": bytes})
        }
        SpeechOutcome::Failed(e) => {
            json!({"status": "failed", "provider": e.provider, "code": e.code, "message": e.message})
        }
    };
    let speech_ok = matches!(
        spoken.speech,
        SpeechOutcome::Synthesized { .. } | SpeechOutcome::AlreadyQueued
    );
    Ok(json!({
        "ok": all_sent && speech_ok,
        "team_id": team,
        "channel": args.channel,
        "thread_ts": args.thread,
        "turn_id": turn_id,
        "fake_providers": providers.fake,
        "tts_provider": tts.primary_provider(),
        "speech": speech,
        "newly_enqueued": spoken.enqueued.queued,
        "already_enqueued": spoken.enqueued.duplicates,
        "sent_now": rows.iter().filter(|r| r["sent_now"] == json!(true)).count(),
        "sends": rows,
        "audio_released": released,
    }))
}

fn print_speak(v: &Value) {
    let place = match v["thread_ts"].as_str() {
        Some(ts) => format!(
            "{} (thread {ts})",
            v["channel"].as_str().unwrap_or_default()
        ),
        None => v["channel"].as_str().unwrap_or_default().to_string(),
    };
    println!(
        "Spoken reply to {place} in {}: turn {}; text-to-speech: {}{}",
        v["team_id"].as_str().unwrap_or_default(),
        v["turn_id"].as_str().unwrap_or_default(),
        v["tts_provider"].as_str().unwrap_or_default(),
        if v["fake_providers"] == json!(true) {
            " (offline test fake)"
        } else {
            ""
        },
    );
    let s = &v["speech"];
    match s["status"].as_str().unwrap_or_default() {
        "synthesized" => println!(
            "  speech: synthesized {} bytes with {}{}",
            s["bytes"],
            s["provider"].as_str().unwrap_or_default(),
            s["switched_from"]
                .as_str()
                .map(|f| format!(" (switched from {f}: credit exhausted)"))
                .unwrap_or_default()
        ),
        "already_queued" => {
            println!("  speech: already in the outbox for this turn; not synthesised again")
        }
        "failed" => println!(
            "  speech: FAILED ({} {}: {}); the text was sent with a note",
            s["provider"].as_str().unwrap_or_default(),
            s["code"].as_str().unwrap_or_default(),
            s["message"].as_str().unwrap_or_default()
        ),
        other => println!("  speech: {other}"),
    }
    for r in v["sends"].as_array().into_iter().flatten() {
        println!(
            "  {:<42} {:<7} {:<11} {}{}",
            r["idempotency_key"].as_str().unwrap_or(""),
            r["operation"].as_str().unwrap_or(""),
            r["status"].as_str().unwrap_or(""),
            r["provider_message_id"].as_str().unwrap_or("-"),
            if r["sent_now"] == json!(true) {
                "  (sent now)"
            } else if r["already_enqueued"] == json!(true) {
                "  (already enqueued; not sent again)"
            } else {
                ""
            },
        );
        if let Some(e) = r["last_error"].as_str() {
            println!("      last error: {e}");
        }
    }
    println!(
        "  sent now: {}; stored audio removed: {}",
        v["sent_now"],
        match v["audio_released"].as_bool() {
            Some(b) => b.to_string(),
            None => "n/a (no stored audio)".into(),
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_speech_override_is_debug_only_and_validated() {
        if cfg!(debug_assertions) {
            assert!(test_providers(None).is_none());
            assert!(test_providers(Some("  ")).is_none());
            let p = test_providers(Some("ok:hello")).unwrap().unwrap();
            assert!(p.fake && p.tts.is_some());
            assert_eq!(p.stt.primary_provider(), "deepgram");
            assert!(test_providers(Some("bogus")).unwrap().is_err());
        } else {
            assert!(test_providers(Some("ok:hello")).is_none());
        }
    }

    #[test]
    fn default_turn_id_is_stable_and_separates_inputs() {
        let a = default_turn_id("T00000001", "C00000001", None, "hi");
        assert_eq!(a, default_turn_id("T00000001", "C00000001", None, "hi"));
        assert!(a.starts_with("cli-voice-"));
        assert_ne!(
            a,
            default_turn_id("T00000001", "C00000001", Some("1.2"), "hi")
        );
        assert_ne!(a, default_turn_id("T00000001", "C0000000", None, "1hi"));
    }
}
