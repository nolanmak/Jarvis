//! #1297 — spoken replies: when the turn asks for one, the answer is
//! synthesised through the speech seam (sidecar fallback rule) and delivered
//! as an uploaded audio file plus the full text mirror, through the #1294
//! outbox with per-part idempotency keys, exactly once.
//!
//! The TTS provider is a scripted fake producing a tiny synthetic WAV; Slack
//! is the recording fake; the store is a temporary file.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use augmentagent_channel_slack::delivery::{
    part_idempotency_key, AnswerFile, PartKind, SlackOutboxDispatcher,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::{RecordedCall, RecordingSlackWebApi};
use augmentagent_channel_slack::voice::audio::{wav_duration_ms, wav_from_pcm16};
use augmentagent_channel_slack::voice::fake::{ScriptedTts, TtsScript};
use augmentagent_channel_slack::voice::reply::{
    enqueue_spoken_answer, release_spoken_audio, speakable_text, spoken_audio_key, ReplyMode,
    SpeechOutcome, SpokenAnswer, SpokenReplyOptions, SPOKEN_REPLY_FILENAME,
};
use augmentagent_channel_slack::voice::speech::{AudioFormat, SynthesizedAudio};
use augmentagent_channel_slack::voice::TtsStack;
use augmentagent_store::delivery::SendStatus;
use augmentagent_store::{Store, SurfaceConversationRef};

const T0: i64 = 1_700_000_000_000;
const CHANNEL: &str = "C00000001";
const THREAD: &str = "1700000000.000100";

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new("T00000001", None).unwrap()
}

fn thread() -> SurfaceConversationRef {
    workspace().conversation(CHANNEL, Some(THREAD)).unwrap()
}

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    opts: SpokenReplyOptions,
    api: RecordingSlackWebApi,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let root = dir.path().join("state dir ü").join("slack-voice-replies");
    Fx {
        store,
        opts: SpokenReplyOptions::new(root),
        api: RecordingSlackWebApi::default(),
        _dir: dir,
    }
}

async fn drain(fx: &Fx, now_ms: i64) {
    SlackOutboxDispatcher::new(&fx.store, &fx.api, &workspace())
        .drain(now_ms)
        .await
        .unwrap();
}

fn posts(api: &RecordingSlackWebApi) -> Vec<String> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::PostMessage(p) => Some(p.text),
            _ => None,
        })
        .collect()
}

fn uploads(api: &RecordingSlackWebApi) -> Vec<(String, Option<String>, usize)> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::UploadFile {
                filename,
                thread_ts,
                bytes,
                ..
            } => Some((filename, thread_ts, bytes)),
            _ => None,
        })
        .collect()
}

const ANSWER: &str = "**Tomorrow** you have two meetings:\n\n- 9:00 standup\n- 14:00 review\n\n```sh\nrm -rf /tmp/x\n```\nSee [the doc](https://example.com/doc).";

#[tokio::test]
async fn spoken_reply_is_an_audio_upload_plus_the_text_mirror_delivered_once() {
    let fx = fx();
    let tts = ScriptedTts::new("deepgram", vec![TtsScript::Wav(400)]);
    let stack = TtsStack::new(Arc::new(tts.clone()));
    let answer = SpokenAnswer {
        turn_id: "turn-voice-1",
        markdown: ANSWER,
        files: &[],
        mode: ReplyMode::Spoken,
    };

    let first = enqueue_spoken_answer(&fx.store, &thread(), &answer, Some(&stack), &fx.opts, T0)
        .await
        .unwrap();
    match &first.speech {
        SpeechOutcome::Synthesized {
            provider,
            switched_from,
            bytes,
        } => {
            assert_eq!(provider, "deepgram");
            assert_eq!(*switched_from, None);
            assert!(*bytes > 44);
        }
        other => panic!("{other:?}"),
    }
    let audio = first.audio_path.clone().expect("audio stored");
    assert!(audio.exists());
    assert_eq!(
        std::fs::metadata(&audio).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(audio.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let keys: Vec<&str> = first
        .enqueued
        .sends
        .iter()
        .map(|s| s.idempotency_key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            part_idempotency_key("turn-voice-1", PartKind::Text, 0),
            spoken_audio_key("turn-voice-1"),
        ]
    );

    // The provider was asked to speak prose, not Markdown or code.
    let spoken = tts.seen();
    assert_eq!(spoken.len(), 1);
    assert!(
        spoken[0].contains("Tomorrow you have two meetings"),
        "{}",
        spoken[0]
    );
    assert!(
        !spoken[0].contains("**") && !spoken[0].contains("rm -rf"),
        "{}",
        spoken[0]
    );

    drain(&fx, T0).await;
    let texts = posts(&fx.api);
    assert_eq!(texts.len(), 1);
    assert!(
        texts[0].contains("*Tomorrow*"),
        "mrkdwn mirror: {}",
        texts[0]
    );
    assert!(
        texts[0].contains("rm -rf"),
        "full text mirror: {}",
        texts[0]
    );
    let ups = uploads(&fx.api);
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].0, SPOKEN_REPLY_FILENAME);
    assert_eq!(ups[0].1.as_deref(), Some(THREAD), "same thread");

    // A restart of the same turn: no new synthesis, nothing sent twice.
    let again = enqueue_spoken_answer(
        &fx.store,
        &thread(),
        &answer,
        Some(&stack),
        &fx.opts,
        T0 + 1,
    )
    .await
    .unwrap();
    assert_eq!(again.speech, SpeechOutcome::AlreadyQueued);
    assert_eq!(again.enqueued.queued, 0);
    assert_eq!(again.enqueued.duplicates, 2);
    drain(&fx, T0 + 2).await;
    assert_eq!(tts.seen().len(), 1, "synthesised once");
    assert_eq!(posts(&fx.api).len(), 1);
    assert_eq!(uploads(&fx.api).len(), 1);

    // Once delivered, the stored audio can go.
    assert!(release_spoken_audio(
        &fx.store,
        &workspace().account(),
        "turn-voice-1",
        &fx.opts.root
    )
    .unwrap());
    assert!(!audio.exists());
}

#[tokio::test]
async fn text_mode_never_calls_the_speech_provider() {
    let fx = fx();
    let tts = ScriptedTts::new("deepgram", vec![TtsScript::Wav(100)]);
    let stack = TtsStack::new(Arc::new(tts.clone()));
    let answer = SpokenAnswer {
        turn_id: "turn-text-1",
        markdown: "plain answer",
        files: &[],
        mode: ReplyMode::Text,
    };

    let out = enqueue_spoken_answer(&fx.store, &thread(), &answer, Some(&stack), &fx.opts, T0)
        .await
        .unwrap();

    assert_eq!(out.speech, SpeechOutcome::NotRequested);
    assert!(out.audio_path.is_none());
    assert_eq!(out.enqueued.sends.len(), 1);
    assert!(tts.seen().is_empty());
}

#[test]
fn reply_mode_defaults_to_text_and_parses_owner_requests() {
    assert_eq!(ReplyMode::default(), ReplyMode::Text);
    assert_eq!("spoken".parse::<ReplyMode>().unwrap(), ReplyMode::Spoken);
    assert_eq!("voice".parse::<ReplyMode>().unwrap(), ReplyMode::Spoken);
    assert_eq!("text".parse::<ReplyMode>().unwrap(), ReplyMode::Text);
    assert!("loud".parse::<ReplyMode>().is_err());
}

#[tokio::test]
async fn generated_files_follow_the_spoken_audio() {
    let fx = fx();
    let doc = fx.opts.root.parent().unwrap().join("report.pdf");
    std::fs::create_dir_all(doc.parent().unwrap()).unwrap();
    std::fs::write(&doc, b"%PDF-1.4 synthetic").unwrap();
    let files = [AnswerFile {
        path: doc,
        filename: None,
        title: None,
        alt_text: None,
    }];
    let stack = TtsStack::new(Arc::new(ScriptedTts::new(
        "deepgram",
        vec![TtsScript::Wav(100)],
    )));
    let answer = SpokenAnswer {
        turn_id: "turn-files-1",
        markdown: "here is the report",
        files: &files,
        mode: ReplyMode::Spoken,
    };

    enqueue_spoken_answer(&fx.store, &thread(), &answer, Some(&stack), &fx.opts, T0)
        .await
        .unwrap();
    drain(&fx, T0).await;

    let names: Vec<String> = uploads(&fx.api).into_iter().map(|u| u.0).collect();
    assert_eq!(
        names,
        vec![SPOKEN_REPLY_FILENAME.to_string(), "report.pdf".into()]
    );
}

#[tokio::test]
async fn tts_failure_still_delivers_the_text_with_a_clear_note() {
    let fx = fx();
    let tts = ScriptedTts::new("deepgram", vec![TtsScript::Fail("500".into())]);
    let stack = TtsStack::new(Arc::new(tts.clone()));
    let answer = SpokenAnswer {
        turn_id: "turn-fail-1",
        markdown: "the answer",
        files: &[],
        mode: ReplyMode::Spoken,
    };

    let out = enqueue_spoken_answer(&fx.store, &thread(), &answer, Some(&stack), &fx.opts, T0)
        .await
        .unwrap();

    match &out.speech {
        SpeechOutcome::Failed(e) => {
            assert_eq!(e.provider, "deepgram");
            assert_eq!(e.code, "500");
        }
        other => panic!("{other:?}"),
    }
    assert!(out.audio_path.is_none());
    drain(&fx, T0).await;
    let texts = posts(&fx.api);
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("the answer"));
    assert!(
        texts[0].contains("Spoken reply unavailable"),
        "{}",
        texts[0]
    );
    assert!(texts[0].contains("deepgram"), "{}", texts[0]);
    assert!(uploads(&fx.api).is_empty());
}

#[tokio::test]
async fn missing_tts_provider_is_reported_not_silent() {
    let fx = fx();
    let answer = SpokenAnswer {
        turn_id: "turn-none-1",
        markdown: "the answer",
        files: &[],
        mode: ReplyMode::Spoken,
    };

    let out = enqueue_spoken_answer(&fx.store, &thread(), &answer, None, &fx.opts, T0)
        .await
        .unwrap();

    assert!(matches!(out.speech, SpeechOutcome::Failed(ref e) if e.code == "not_configured"));
    drain(&fx, T0).await;
    assert!(posts(&fx.api)[0].contains("Spoken reply unavailable"));
}

#[tokio::test]
async fn exhausted_tts_switches_to_the_alternate() {
    let fx = fx();
    let primary = ScriptedTts::new("elevenlabs", vec![TtsScript::Fail("quota_exceeded".into())]);
    let alternate = ScriptedTts::new("deepgram", vec![TtsScript::Pcm24k(200)]);
    let stack =
        TtsStack::new(Arc::new(primary.clone())).with_alternate(Arc::new(alternate.clone()));
    let answer = SpokenAnswer {
        turn_id: "turn-switch-1",
        markdown: "switch please",
        files: &[],
        mode: ReplyMode::Spoken,
    };

    let out = enqueue_spoken_answer(&fx.store, &thread(), &answer, Some(&stack), &fx.opts, T0)
        .await
        .unwrap();

    match &out.speech {
        SpeechOutcome::Synthesized {
            provider,
            switched_from,
            ..
        } => {
            assert_eq!(provider, "deepgram");
            assert_eq!(switched_from.as_deref(), Some("elevenlabs"));
        }
        other => panic!("{other:?}"),
    }
    // Raw 24 kHz PCM from the provider is stored as a playable WAV.
    let bytes = std::fs::read(out.audio_path.unwrap()).unwrap();
    assert_eq!(wav_duration_ms(&bytes), Some((200, 24_000)));
    assert_eq!(primary.seen().len(), 1);
}

#[tokio::test]
async fn stored_audio_is_kept_until_its_upload_is_settled() {
    let fx = fx();
    let stack = TtsStack::new(Arc::new(ScriptedTts::new(
        "deepgram",
        vec![TtsScript::Wav(100)],
    )));
    let answer = SpokenAnswer {
        turn_id: "turn-keep-1",
        markdown: "text first",
        files: &[],
        mode: ReplyMode::Spoken,
    };
    let out = enqueue_spoken_answer(&fx.store, &thread(), &answer, Some(&stack), &fx.opts, T0)
        .await
        .unwrap();
    let audio = out.audio_path.unwrap();

    // Queued, not yet uploaded: the dispatcher still needs the file.
    assert!(!release_spoken_audio(
        &fx.store,
        &workspace().account(),
        "turn-keep-1",
        &fx.opts.root
    )
    .unwrap());
    assert!(audio.exists());
    // Unknown turn: nothing to release, nothing removed.
    assert!(!release_spoken_audio(
        &fx.store,
        &workspace().account(),
        "turn-other",
        &fx.opts.root
    )
    .unwrap());

    drain(&fx, T0).await;
    let row = fx
        .store
        .outbound_sends_with_key_prefix(
            &workspace().account(),
            &spoken_audio_key("turn-keep-1"),
            &[],
        )
        .unwrap()
        .remove(0);
    assert_eq!(row.status, SendStatus::Sent);
    assert!(release_spoken_audio(
        &fx.store,
        &workspace().account(),
        "turn-keep-1",
        &fx.opts.root
    )
    .unwrap());
    assert!(!audio.exists());
}

#[test]
fn speakable_text_drops_markup_code_and_urls() {
    let s = speakable_text(ANSWER, 10_000);
    assert!(s.starts_with("Tomorrow you have two meetings:"), "{s}");
    assert!(s.contains("9:00 standup"), "{s}");
    assert!(s.contains("the code is in the text reply"), "{s}");
    assert!(s.contains("See the doc."), "{s}");
    assert!(!s.contains("https://"), "{s}");
    assert!(
        !s.contains('*') && !s.contains('`') && !s.contains('#'),
        "{s}"
    );

    let long = "word ".repeat(5_000);
    let cut = speakable_text(&long, 1_000);
    assert!(cut.chars().count() <= 1_100, "{}", cut.len());
    assert!(cut.ends_with("The rest is in the text reply."), "{cut}");
}

#[test]
fn synthesized_audio_formats_become_uploadable_files() {
    let wav = wav_from_pcm16(&[0i16; 160], 16_000);
    let a = SynthesizedAudio {
        bytes: wav.clone(),
        format: AudioFormat::Wav,
    };
    assert_eq!(a.to_file_bytes().unwrap(), wav);
    let pcm = SynthesizedAudio {
        bytes: vec![0u8; 480],
        format: AudioFormat::Pcm16 {
            sample_rate: 24_000,
        },
    };
    let file = pcm.to_file_bytes().unwrap();
    assert_eq!(wav_duration_ms(&file), Some((10, 24_000)));
    let odd = SynthesizedAudio {
        bytes: vec![0u8; 3],
        format: AudioFormat::Pcm16 {
            sample_rate: 24_000,
        },
    };
    assert!(odd.to_file_bytes().is_err());
    let empty = SynthesizedAudio {
        bytes: vec![],
        format: AudioFormat::Mp3,
    };
    assert!(empty.to_file_bytes().is_err());
}
