//! #1297 — owner voice clips and uploaded audio in Slack become a normal
//! turn: downloaded with the inbound pipeline's limits, decoded to 16 kHz
//! mono WAV by `ffmpeg` (resolved like the #1293 converters), transcribed
//! through the speech seam with the sidecar's fallback rule, and shown back
//! to the owner as a transcript line.
//!
//! Audio fixtures are tiny synthetic WAVs generated here; `ffmpeg` is a fake
//! shell script and speech providers are scripted fakes, so the tests are
//! identical on Linux and macOS and never call a paid provider.

#![cfg(unix)]

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use augmentagent_channel_slack::inbound::{
    prepare_inbound, prepare_inbound_with_voice, InboundMessage, InboundOptions,
};
use augmentagent_channel_slack::owner::{
    admit, AdmitOutcome, OwnerInput, OwnerInputSink, SlackOwnerAuthorizer,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::{
    file_refs, parse_envelope, Envelope, EventEnvelope, FileRef, SlackEvent,
};
use augmentagent_channel_slack::transport::web::RecordingSlackWebApi;
use augmentagent_channel_slack::voice::audio::{
    clip_format, wav_duration_ms, wav_from_pcm16, ClipFormat, ClipMedia, STT_SAMPLE_RATE,
};
use augmentagent_channel_slack::voice::fake::{ScriptedStt, SttScript};
use augmentagent_channel_slack::voice::{ClipLimits, SttStack, VoiceInbound};
use augmentagent_docs::inbound::RejectReason;
use augmentagent_docs::ConvertOptions;
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::Store;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const CONTROL: &str = "C00000001";
const T0: i64 = 1_700_000_000_000;

/// `ms` of a quiet 440 Hz tone, 16 kHz mono 16-bit: the format the fake
/// `ffmpeg` passes through unchanged.
fn tone_wav(ms: u64) -> Vec<u8> {
    let n = (STT_SAMPLE_RATE as u64 * ms / 1000) as usize;
    let samples: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f32 / STT_SAMPLE_RATE as f32;
            ((t * 440.0 * std::f32::consts::TAU).sin() * 3000.0) as i16
        })
        .collect();
    wav_from_pcm16(&samples, STT_SAMPLE_RATE)
}

fn url(id: &str, name: &str) -> String {
    format!("https://files.slack.com/files-pri/T00000001-{id}/download/{name}")
}

fn clip(id: &str, name: &str, mimetype: &str, bytes: usize) -> FileRef {
    let v = json!({"files": [{
        "id": id,
        "name": name,
        "mimetype": mimetype,
        "size": bytes,
        "url_private": url(id, "private"),
        "url_private_download": url(id, name),
        "mode": "hosted",
    }]});
    file_refs(&v).remove(0)
}

/// A Slack audio clip as the file object documents it (`subtype`,
/// `media_display_type`, `duration_ms`).
fn slack_audio_clip(id: &str, bytes: usize, duration_ms: u64) -> FileRef {
    let v = json!({"files": [{
        "id": id,
        "name": "audio_message.webm",
        "title": "Audio clip",
        "mimetype": "audio/webm",
        "filetype": "webm",
        "subtype": "slack_audio",
        "media_display_type": "audio",
        "duration_ms": duration_ms,
        "size": bytes,
        "url_private": url(id, "private"),
        "url_private_download": url(id, "audio_message.webm"),
        "mode": "hosted",
    }]});
    file_refs(&v).remove(0)
}

fn fake_tool(dir: &Path, name: &str, script: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Copies `-i <in>` to the last argument and logs its arguments.
const FAKE_FFMPEG: &str = r#"in=""; out=""; prev=""
for a in "$@"; do
  if [ "$prev" = "-i" ]; then in="$a"; fi
  prev="$a"; out="$a"
done
echo "$@" >> "$(dirname "$0")/ffmpeg.log"
cp "$in" "$out""#;

struct Fixture {
    _state: tempfile::TempDir,
    root: PathBuf,
    tools: tempfile::TempDir,
    opts: InboundOptions,
    api: RecordingSlackWebApi,
}

impl Fixture {
    fn tool_opts(&self) -> ConvertOptions {
        ConvertOptions {
            search_path: Some(self.tools.path().as_os_str().to_owned()),
            fallback_dirs: vec![],
            timeout: Duration::from_secs(5),
            ..ConvertOptions::default()
        }
    }

    fn voice<'a>(&self, stt: &'a SttStack) -> VoiceInbound<'a> {
        VoiceInbound {
            stt,
            tools: self.tool_opts(),
            limits: ClipLimits::default(),
        }
    }

    fn ffmpeg_log(&self) -> String {
        std::fs::read_to_string(self.tools.path().join("ffmpeg.log")).unwrap_or_default()
    }
}

fn fixture() -> Fixture {
    let state = tempfile::tempdir().unwrap();
    // Spaces and Unicode, like a macOS home can have.
    let root = state.path().join("state dir ü").join("slack-inbound");
    let tools = tempfile::tempdir().unwrap();
    fake_tool(tools.path(), "ffmpeg", FAKE_FFMPEG);
    let opts = InboundOptions::new(root.clone());
    Fixture {
        _state: state,
        root,
        tools,
        opts,
        api: RecordingSlackWebApi::default(),
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

async fn run(fx: &Fixture, text: &str, files: &[FileRef], stt: &SttStack) -> InboundMessage {
    prepare_inbound_with_voice(
        &fx.api,
        text,
        files,
        &fx.opts,
        Some(&fx.voice(stt)),
        &CancellationToken::new(),
    )
    .await
    .expect("prepare")
}

fn only_reason(msg: &InboundMessage) -> String {
    assert_eq!(msg.rejected.len(), 1, "{:?}", msg.rejected);
    match &msg.rejected[0].reason {
        RejectReason::Unavailable(why) => why.clone(),
        other => panic!("expected an owner-facing reason, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Recognising clips
// ---------------------------------------------------------------------------

#[test]
fn slack_clip_fields_are_parsed_from_the_file_object() {
    let f = slack_audio_clip("F1", 1234, 4_200);
    assert_eq!(f.subtype.as_deref(), Some("slack_audio"));
    assert_eq!(f.media_display_type.as_deref(), Some("audio"));
    assert_eq!(f.duration_ms, Some(4_200));
}

#[test]
fn audio_and_video_clips_are_recognised_and_other_files_are_not() {
    let supported = [
        ("clip.webm", "audio/webm", ClipMedia::Audio),
        ("voice.m4a", "audio/mp4", ClipMedia::Audio),
        ("voice.m4a", "audio/x-m4a", ClipMedia::Audio),
        ("song.mp3", "audio/mpeg", ClipMedia::Audio),
        ("note.wav", "audio/wav", ClipMedia::Audio),
        ("note.wav", "audio/x-wav", ClipMedia::Audio),
        ("memo.ogg", "audio/ogg", ClipMedia::Audio),
        ("memo.flac", "audio/flac", ClipMedia::Audio),
        ("video_clip.mp4", "video/mp4", ClipMedia::Video),
        ("video_clip.webm", "video/webm", ClipMedia::Video),
        ("screen.mov", "video/quicktime", ClipMedia::Video),
        // Generic MIME: the extension decides.
        ("voice.m4a", "application/octet-stream", ClipMedia::Audio),
    ];
    for (name, mime, media) in supported {
        let f = clip("F1", name, mime, 10);
        assert_eq!(
            clip_format(&f),
            Some(ClipFormat::Supported(media)),
            "{name} {mime}"
        );
    }
    // A Slack clip is recognised from its subtype even with an odd MIME.
    let mut f = slack_audio_clip("F1", 10, 1000);
    f.mimetype = Some("application/octet-stream".into());
    f.name = Some("audio_message".into());
    assert_eq!(
        clip_format(&f),
        Some(ClipFormat::Supported(ClipMedia::Audio))
    );

    for (name, mime) in [
        ("voice.amr", "audio/amr"),
        ("track.wma", "audio/x-ms-wma"),
        ("film.avi", "video/x-msvideo"),
    ] {
        match clip_format(&clip("F1", name, mime, 10)) {
            Some(ClipFormat::Unsupported(detail)) => assert!(detail.contains(mime), "{detail}"),
            other => panic!("{name}: expected unsupported, got {other:?}"),
        }
    }
    for (name, mime) in [
        ("photo.png", "image/png"),
        ("notes.md", "text/markdown"),
        ("r.pdf", "application/pdf"),
    ] {
        assert_eq!(clip_format(&clip("F1", name, mime, 10)), None, "{name}");
    }
}

// ---------------------------------------------------------------------------
// Transcription into the turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn clip_is_transcribed_into_turn_text_with_a_transcript_line() {
    let fx = fixture();
    let wav = tone_wav(1_000);
    let f = slack_audio_clip("F1", wav.len(), 1_000);
    fx.api.add_file(&url("F1", "audio_message.webm"), wav);
    let stt = ScriptedStt::new(
        "deepgram",
        vec![SttScript::Text("what is on my calendar today".into())],
    );
    let stack = SttStack::new(Arc::new(stt.clone()));

    let msg = run(&fx, "", std::slice::from_ref(&f), &stack).await;

    assert!(msg.rejected.is_empty(), "{:?}", msg.rejected);
    assert!(msg.starts_turn());
    assert_eq!(msg.turn_text(), "what is on my calendar today");
    assert_eq!(msg.prompt, "what is on my calendar today");
    assert_eq!(msg.transcripts.len(), 1);
    let t = &msg.transcripts[0];
    assert_eq!(t.file_id, "F1");
    assert_eq!(t.provider, "deepgram");
    assert_eq!(t.switched_from, None);
    assert_eq!(t.duration_ms, 1_000);
    let line = msg.transcript_notice().expect("transcript line");
    assert!(line.contains("what is on my calendar today"), "{line}");
    assert!(line.contains("0:01"), "{line}");

    // The provider got one 16 kHz mono WAV, decoded by ffmpeg with the
    // duration bound applied.
    let seen = stt.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].sample_rate, STT_SAMPLE_RATE);
    assert_eq!(seen[0].duration_ms, 1_000);
    let log = fx.ffmpeg_log();
    assert!(log.contains("-ar 16000") && log.contains("-ac 1"), "{log}");
    assert!(log.contains("-t "), "decode is bounded in time: {log}");

    // Audio never outlives transcription: the message dir holds nothing.
    let dir = msg.dir().map(Path::to_path_buf);
    if let Some(dir) = &dir {
        assert!(entries(dir).is_empty(), "{:?}", entries(dir));
    }
    msg.cleanup().unwrap();
    assert!(entries(&fx.root).is_empty());
}

#[tokio::test]
async fn typed_text_images_and_a_clip_share_one_turn() {
    let fx = fixture();
    let wav = tone_wav(500);
    let f_clip = clip("F2", "voice memo ü.m4a", "audio/mp4", wav.len());
    fx.api.add_file(&url("F2", "voice memo ü.m4a"), wav);
    let png = vec![0x89, b'P', b'N', b'G'];
    let f_img = clip("F1", "photo.png", "image/png", png.len());
    fx.api.add_file(&url("F1", "photo.png"), png);
    let stack = SttStack::new(Arc::new(ScriptedStt::new(
        "deepgram",
        vec![SttScript::Text("and summarise this photo".into())],
    )));

    let msg = run(&fx, "look at this", &[f_img, f_clip], &stack).await;

    assert_eq!(msg.turn_text(), "look at this\n\nand summarise this photo");
    assert!(msg
        .prompt
        .starts_with("look at this\n\nand summarise this photo\n\n"));
    assert!(msg.prompt.contains("IMAGE: "), "{}", msg.prompt);
    assert_eq!(msg.images.len(), 1);
    assert_eq!(msg.transcripts[0].original_name, "voice memo ü.m4a");
}

#[tokio::test]
async fn empty_transcript_is_reported_and_starts_no_turn() {
    let fx = fixture();
    let wav = tone_wav(300);
    let f = clip("F1", "silence.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "silence.wav"), wav);
    let stack = SttStack::new(Arc::new(ScriptedStt::new(
        "deepgram",
        vec![SttScript::Text("   ".into())],
    )));

    let msg = run(&fx, "", &[f], &stack).await;

    assert!(!msg.starts_turn());
    assert!(
        only_reason(&msg).contains("no speech"),
        "{:?}",
        msg.rejected
    );
}

// ---------------------------------------------------------------------------
// Rejections: codec, limits, decode, provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unsupported_codec_is_rejected_before_download_with_a_clear_reason() {
    let fx = fixture();
    let f = clip("F1", "voice.amr", "audio/amr", 100);
    fx.api.add_file(&url("F1", "voice.amr"), vec![0; 100]);
    let stt = ScriptedStt::new("deepgram", vec![]);
    let stack = SttStack::new(Arc::new(stt.clone()));

    let msg = run(&fx, "", &[f], &stack).await;

    let why = only_reason(&msg);
    assert!(why.contains("unsupported audio format"), "{why}");
    assert!(why.contains("audio/amr"), "{why}");
    assert!(why.contains("m4a"), "names what works: {why}");
    assert!(!msg.starts_turn());
    assert!(fx.api.calls().is_empty(), "nothing downloaded");
    assert!(stt.seen().is_empty(), "provider never called");
    let notice = msg.rejection_notice().unwrap();
    assert!(notice.contains("voice.amr"), "{notice}");
}

#[tokio::test]
async fn undecodable_audio_is_rejected_and_the_provider_is_not_called() {
    let fx = fixture();
    fake_tool(
        fx.tools.path(),
        "ffmpeg",
        "echo 'Invalid data found when processing input' >&2; exit 1",
    );
    let f = clip("F1", "broken.webm", "audio/webm", 64);
    fx.api.add_file(&url("F1", "broken.webm"), vec![7; 64]);
    let stt = ScriptedStt::new("deepgram", vec![]);
    let stack = SttStack::new(Arc::new(stt.clone()));

    let msg = run(&fx, "", &[f], &stack).await;

    let why = only_reason(&msg);
    assert!(why.contains("couldn't decode"), "{why}");
    assert!(why.contains("Invalid data"), "{why}");
    assert!(stt.seen().is_empty());
    assert!(entries(msg.dir().unwrap()).is_empty(), "download removed");
}

#[tokio::test]
async fn missing_ffmpeg_names_the_install_for_each_host() {
    let fx = fixture();
    std::fs::remove_file(fx.tools.path().join("ffmpeg")).unwrap();
    let wav = tone_wav(200);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let stt = ScriptedStt::new("deepgram", vec![]);
    let stack = SttStack::new(Arc::new(stt.clone()));

    let msg = run(&fx, "", &[f], &stack).await;

    let why = only_reason(&msg);
    assert!(why.contains("ffmpeg is not installed"), "{why}");
    assert!(why.contains("brew install ffmpeg"), "{why}");
    assert!(why.contains("apt install ffmpeg"), "{why}");
    assert!(stt.seen().is_empty());
}

#[tokio::test]
async fn stuck_ffmpeg_is_stopped_at_the_timeout() {
    let fx = fixture();
    fake_tool(fx.tools.path(), "ffmpeg", "sleep 30");
    let wav = tone_wav(200);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let stack = SttStack::new(Arc::new(ScriptedStt::new("deepgram", vec![])));
    let mut voice = fx.voice(&stack);
    voice.tools.timeout = Duration::from_millis(300);

    let started = std::time::Instant::now();
    let msg = prepare_inbound_with_voice(
        &fx.api,
        "",
        &[f],
        &fx.opts,
        Some(&voice),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        only_reason(&msg).contains("timed out"),
        "{:?}",
        msg.rejected
    );
}

#[tokio::test]
async fn size_and_duration_limits_are_enforced_before_and_after_decoding() {
    let fx = fixture();
    let limits = ClipLimits {
        max_bytes: 50_000,
        max_duration: Duration::from_secs(2),
        ..ClipLimits::default()
    };
    let stt = ScriptedStt::new("deepgram", vec![SttScript::Text("unused".into())]);
    let stack = SttStack::new(Arc::new(stt.clone()));
    let mut voice = fx.voice(&stack);
    voice.limits = limits;

    // Declared too big, declared too long: refused before any download.
    let big = clip("F1", "big.wav", "audio/wav", 60_000);
    let long = slack_audio_clip("F2", 1_000, 3_000);
    // No declared duration, but the decoded audio is 3 s long.
    let wav = tone_wav(3_000);
    let long_decoded = clip("F3", "long.wav", "audio/wav", wav.len().min(49_000));
    fx.api.add_file(&url("F3", "long.wav"), wav);

    let msg = prepare_inbound_with_voice(
        &fx.api,
        "",
        &[big, long, long_decoded],
        &fx.opts,
        Some(&voice),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(msg.rejected.len(), 3, "{:?}", msg.rejected);
    assert!(matches!(
        msg.rejected[0].reason,
        RejectReason::Oversize {
            size: 60_000,
            limit: 50_000
        }
    ));
    let reasons: Vec<String> = msg.rejected[1..]
        .iter()
        .map(|r| match &r.reason {
            RejectReason::Unavailable(w) => w.clone(),
            RejectReason::Oversize { .. } => "oversize".into(),
            other => format!("{other:?}"),
        })
        .collect();
    assert!(reasons[0].contains("longer than 0:02"), "{reasons:?}");
    // The streamed download cap is the byte limit; the 3 s WAV (96 KB) is
    // cut off while streaming, or, when under the cap, fails the decoded
    // duration check. Either way it never reaches the provider.
    assert!(
        reasons[1].contains("longer than 0:02") || reasons[1] == "oversize",
        "{reasons:?}"
    );
    assert!(stt.seen().is_empty());
}

#[tokio::test]
async fn decoded_duration_over_the_limit_is_rejected() {
    let fx = fixture();
    let stt = ScriptedStt::new("deepgram", vec![SttScript::Text("unused".into())]);
    let stack = SttStack::new(Arc::new(stt.clone()));
    let mut voice = fx.voice(&stack);
    voice.limits.max_duration = Duration::from_secs(1);
    let wav = tone_wav(1_500);
    let f = clip("F1", "long.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "long.wav"), wav);

    let msg = prepare_inbound_with_voice(
        &fx.api,
        "",
        &[f],
        &fx.opts,
        Some(&voice),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(
        only_reason(&msg).contains("longer than 0:01"),
        "{:?}",
        msg.rejected
    );
    assert!(stt.seen().is_empty());
}

#[tokio::test]
async fn clips_past_the_per_message_cap_are_skipped() {
    let fx = fixture();
    let stack = SttStack::new(Arc::new(ScriptedStt::new(
        "deepgram",
        vec![SttScript::Text("one".into()), SttScript::Text("two".into())],
    )));
    let mut voice = fx.voice(&stack);
    voice.limits.max_clips = 2;
    let mut files = Vec::new();
    for i in 1..=3 {
        let wav = tone_wav(100);
        let name = format!("c{i}.wav");
        files.push(clip(&format!("F{i}"), &name, "audio/wav", wav.len()));
        fx.api.add_file(&url(&format!("F{i}"), &name), wav);
    }

    let msg = prepare_inbound_with_voice(
        &fx.api,
        "",
        &files,
        &fx.opts,
        Some(&voice),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(msg.transcripts.len(), 2);
    assert_eq!(msg.turn_text(), "one\n\ntwo");
    assert!(only_reason(&msg).contains("more than 2 voice clips"));
}

#[tokio::test]
async fn exhausted_provider_falls_back_to_the_alternate_like_the_sidecar() {
    let fx = fixture();
    let wav = tone_wav(400);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let primary = ScriptedStt::new("deepgram", vec![SttScript::Fail("402".into())]);
    let alternate = ScriptedStt::new("elevenlabs", vec![SttScript::Text("book a table".into())]);
    let stack =
        SttStack::new(Arc::new(primary.clone())).with_alternate(Arc::new(alternate.clone()));

    let msg = run(&fx, "", &[f], &stack).await;

    assert_eq!(msg.turn_text(), "book a table");
    let t = &msg.transcripts[0];
    assert_eq!(t.provider, "elevenlabs");
    assert_eq!(t.switched_from.as_deref(), Some("deepgram"));
    assert!(msg.transcript_notice().unwrap().contains("elevenlabs"));
    assert_eq!(primary.seen().len(), 1);
    assert_eq!(alternate.seen().len(), 1);
}

#[tokio::test]
async fn provider_failure_that_is_not_exhaustion_does_not_switch_and_is_reported() {
    let fx = fixture();
    let wav = tone_wav(400);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let primary = ScriptedStt::new("deepgram", vec![SttScript::Fail("500".into())]);
    let alternate = ScriptedStt::new("elevenlabs", vec![SttScript::Text("never".into())]);
    let stack =
        SttStack::new(Arc::new(primary.clone())).with_alternate(Arc::new(alternate.clone()));

    let msg = run(&fx, "", &[f], &stack).await;

    let why = only_reason(&msg);
    assert!(why.contains("couldn't transcribe"), "{why}");
    assert!(why.contains("deepgram"), "{why}");
    assert!(why.contains("500"), "{why}");
    assert!(why.contains("resend"), "tells the owner what to do: {why}");
    assert!(!msg.starts_turn());
    assert!(
        alternate.seen().is_empty(),
        "only exhaustion switches vendor"
    );
}

#[tokio::test]
async fn both_providers_exhausted_is_a_clear_error() {
    let fx = fixture();
    let wav = tone_wav(400);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let primary = ScriptedStt::new("deepgram", vec![SttScript::Fail("402".into())]);
    let alternate = ScriptedStt::new("elevenlabs", vec![SttScript::Fail("quota_exceeded".into())]);
    let stack = SttStack::new(Arc::new(primary)).with_alternate(Arc::new(alternate));

    let msg = run(&fx, "", &[f], &stack).await;

    let why = only_reason(&msg);
    assert!(why.contains("elevenlabs"), "{why}");
    assert!(why.contains("after switching from deepgram"), "{why}");
}

#[tokio::test]
async fn transcription_is_bounded_in_time() {
    let fx = fixture();
    let wav = tone_wav(200);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let stack = SttStack::new(Arc::new(ScriptedStt::new(
        "deepgram",
        vec![SttScript::Hang],
    )));
    let mut voice = fx.voice(&stack);
    voice.limits.stt_timeout = Duration::from_millis(200);

    let msg = prepare_inbound_with_voice(
        &fx.api,
        "",
        &[f],
        &fx.opts,
        Some(&voice),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let why = only_reason(&msg);
    assert!(why.contains("timed out"), "{why}");
}

#[tokio::test]
async fn cancellation_during_transcription_removes_the_audio() {
    let fx = fixture();
    let wav = tone_wav(200);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);
    let stack = SttStack::new(Arc::new(ScriptedStt::new(
        "deepgram",
        vec![SttScript::Hang],
    )));
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.cancel();
    });

    let r = prepare_inbound_with_voice(
        &fx.api,
        "",
        &[f],
        &fx.opts,
        Some(&fx.voice(&stack)),
        &cancel,
    )
    .await;

    assert!(r.is_err());
    let leftover: Vec<PathBuf> = entries(&fx.root)
        .into_iter()
        .flat_map(|d| entries(&d))
        .collect();
    assert!(leftover.is_empty(), "{leftover:?}");
}

/// Documented limitation: the text-only entry point does not transcribe. A
/// caller that has not opted in with a speech stack keeps #1293's behaviour
/// and the owner sees the clip skipped as unsupported.
#[tokio::test]
async fn without_a_speech_stack_clips_stay_unsupported() {
    let fx = fixture();
    let wav = tone_wav(200);
    let f = clip("F1", "note.wav", "audio/wav", wav.len());
    fx.api.add_file(&url("F1", "note.wav"), wav);

    let msg = prepare_inbound(&fx.api, "", &[f], &fx.opts, &CancellationToken::new())
        .await
        .unwrap();

    assert!(msg.transcripts.is_empty());
    assert!(matches!(
        msg.rejected[0].reason,
        RejectReason::UnsupportedType { .. }
    ));
}

// ---------------------------------------------------------------------------
// Owner-only: admit, then the pipeline
// ---------------------------------------------------------------------------

/// The harness stand-in: it runs the voice pipeline for owner input only.
#[derive(Default)]
struct Admitted {
    inputs: RefCell<Vec<(OwnerInput, Vec<FileRef>, String)>>,
}

impl OwnerInputSink for Admitted {
    fn owner_input(&mut self, input: OwnerInput, envelope: &EventEnvelope) {
        let (files, text) = match &envelope.event {
            SlackEvent::Message(m) | SlackEvent::ThreadReply(m) => {
                (m.files.clone(), m.text.clone())
            }
            _ => (Vec::new(), String::new()),
        };
        self.inputs.borrow_mut().push((input, files, text));
    }
}

fn clip_message(user: &str, file_id: &str, bytes: usize) -> EventEnvelope {
    let event = json!({
        "type": "message",
        "subtype": "file_share",
        "channel": CONTROL,
        "channel_type": "group",
        "user": user,
        "team": TEAM,
        "text": "",
        "ts": "1700000000.000200",
        "files": [{
            "id": file_id,
            "name": "audio_message.webm",
            "mimetype": "audio/webm",
            "filetype": "webm",
            "subtype": "slack_audio",
            "duration_ms": 1000,
            "size": bytes,
            "url_private": url(file_id, "private"),
            "url_private_download": url(file_id, "audio_message.webm"),
            "mode": "hosted",
        }],
    });
    let frame: Value = json!({
        "type": "events_api",
        "envelope_id": format!("env-{file_id}"),
        "payload": {
            "type": "event_callback",
            "team_id": TEAM,
            "api_app_id": "A00000001",
            "event_id": format!("Ev{file_id}"),
            "event_time": 1_700_000_000u64,
            "event": event,
        }
    });
    match parse_envelope(&frame.to_string()).unwrap() {
        Envelope::Event(e) => *e,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn non_owner_audio_never_reaches_the_speech_provider() {
    let fx = fixture();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let ws = SlackWorkspace::new(TEAM, None).unwrap();
    store
        .bind_surface_owner(&ws.owner(OWNER).unwrap(), T0)
        .unwrap();
    store
        .set_surface_control_conversation(
            &ws.conversation(CONTROL, None).unwrap(),
            ControlConversationKind::Channel,
            T0,
        )
        .unwrap();
    let auth = SlackOwnerAuthorizer::load(&store).unwrap();

    let wav = tone_wav(1_000);
    fx.api
        .add_file(&url("FSTRANGER", "audio_message.webm"), wav.clone());
    fx.api
        .add_file(&url("FOWNER", "audio_message.webm"), wav.clone());
    let stt = ScriptedStt::new("deepgram", vec![SttScript::Text("owner speaking".into())]);
    let stack = SttStack::new(Arc::new(stt.clone()));

    let mut harness = Admitted::default();
    let stranger = clip_message(STRANGER, "FSTRANGER", wav.len());
    let outcome = admit(&store, &auth, &stranger, T0, &mut harness).unwrap();
    assert!(
        matches!(outcome, AdmitOutcome::Rejected { .. }),
        "{outcome:?}"
    );
    let owner = clip_message(OWNER, "FOWNER", wav.len());
    assert_eq!(
        admit(&store, &auth, &owner, T0 + 1, &mut harness).unwrap(),
        AdmitOutcome::Dispatched
    );

    // Only what the gate dispatched is run through the voice pipeline.
    let admitted = harness.inputs.take();
    assert_eq!(admitted.len(), 1);
    let mut transcripts = Vec::new();
    for (_input, files, text) in &admitted {
        let msg = run(&fx, text, files, &stack).await;
        transcripts.push(msg.turn_text());
    }

    assert_eq!(transcripts, vec!["owner speaking".to_string()]);
    assert_eq!(
        stt.seen().len(),
        1,
        "exactly the owner's clip was transcribed"
    );
    let downloads: Vec<String> = fx
        .api
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            augmentagent_channel_slack::transport::web::RecordedCall::DownloadFile {
                url_private,
            } => Some(url_private),
            _ => None,
        })
        .collect();
    assert_eq!(downloads.len(), 1);
    assert!(downloads[0].contains("FOWNER"), "{downloads:?}");
    assert!(!downloads.iter().any(|u| u.contains("FSTRANGER")));
}

// ---------------------------------------------------------------------------
// WAV helpers the fakes and the duration check rely on
// ---------------------------------------------------------------------------

#[test]
fn wav_helpers_round_trip_duration() {
    let wav = tone_wav(250);
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(wav_duration_ms(&wav), Some((250, STT_SAMPLE_RATE)));
    assert_eq!(wav_duration_ms(b"not a wav"), None);
}

#[test]
fn durations_are_rounded_for_the_owner() {
    use augmentagent_channel_slack::voice::audio::format_duration;
    assert_eq!(format_duration(3_018), "0:03");
    assert_eq!(format_duration(1_000), "0:01");
    assert_eq!(
        format_duration(400),
        "0:01",
        "a short clip never shows 0:00"
    );
    assert_eq!(format_duration(0), "0:00");
    assert_eq!(format_duration(600_000), "10:00");
}

#[test]
fn an_unconfigured_provider_says_what_is_missing() {
    use augmentagent_channel_slack::voice::speech::{
        FallbackFailure, SpeechError, SpeechOperation, NOT_CONFIGURED,
    };
    let f = FallbackFailure {
        error: SpeechError::new(
            "whisper-cpp",
            SpeechOperation::Stt,
            NOT_CONFIGURED,
            "the whisper.cpp binary or model is not installed (scripts/build-whisper.sh)",
        ),
        switched_from: None,
    };
    let s = f.owner_summary();
    assert!(s.contains("whisper-cpp is not set up"), "{s}");
    assert!(s.contains("scripts/build-whisper.sh"), "{s}");
}
