//! #1297 — `augmentagent slack voice transcribe|speak` end to end: the real
//! binary against a local mock Slack (Web API + loopback file host allowed
//! by the test-only `AUGMENTAGENT_SLACK_TEST_FILE_HOSTS`), a temp database,
//! state dir and insecure credential store, the app installed through
//! `slack app install`. Speech providers are the offline fakes selected by
//! the debug-build-only `AUGMENTAGENT_TEST_SLACK_SPEECH`; `ffmpeg` is a fake
//! first on `PATH`. Tokens, IDs and audio are synthetic.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use mockito::Matcher;
use serde_json::{json, Value};

const APP: &str = "xapp-test-000";
const BOT: &str = "xoxb-test-000";
const TEAM: &str = "T00000001";
const DM: &str = "D00000001";
const TS: &str = "1700000000.000200";
const SPEECH_ENV: &str = "AUGMENTAGENT_TEST_SLACK_SPEECH";

/// 1 s of silence, 16 kHz mono 16-bit: what the fake ffmpeg passes through.
fn wav_1s() -> Vec<u8> {
    let data = vec![0u8; 32_000];
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&16_000u32.to_le_bytes());
    v.extend_from_slice(&32_000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    tools: PathBuf,
    server: mockito::ServerGuard,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home dir ü");
        let tools = root.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let fake = tools.join("ffmpeg");
        std::fs::write(
            &fake,
            "#!/bin/sh\nin=\"\"; out=\"\"; prev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-i\" ]; then in=\"$a\"; fi\n  prev=\"$a\"; out=\"$a\"\ndone\ncp \"$in\" \"$out\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut server = mockito::Server::new();
        mock_install(&mut server);
        let env = Env {
            _tmp: tmp,
            root,
            tools,
            server,
        };
        let out = env.run_stdin(
            &["slack", "app", "install", "--stdin", "--json"],
            &format!("{APP}\n{BOT}\n"),
            None,
        );
        assert!(out.status.success(), "install: {}", text(&out));
        env
    }

    fn host(&self) -> String {
        self.server.url().trim_start_matches("http://").to_string()
    }

    fn state(&self) -> PathBuf {
        self.root.join("state/augmentagent")
    }

    fn cmd(&self, args: &[&str], speech: Option<&str>) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        c.current_dir(&self.root)
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("AUGMENTAGENT_DB", self.root.join("agent.db"))
            .env(
                "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
                self.root.join("cred store"),
            )
            .env("AUGMENTAGENT_SLACK_API_BASE", self.server.url())
            .env("AUGMENTAGENT_SLACK_TEST_FILE_HOSTS", self.host())
            .env("PATH", format!("{}:/usr/bin:/bin", self.tools.display()))
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env("RUST_LOG", "debug")
            .env_remove("AUGMENTAGENT_SLACK_APP_TOKEN")
            .env_remove("AUGMENTAGENT_SLACK_BOT_TOKEN")
            .env_remove("DEEPGRAM_API_KEY")
            .env_remove("ELEVENLABS_API_KEY")
            .env_remove(SPEECH_ENV)
            .args(args);
        if let Some(s) = speech {
            c.env(SPEECH_ENV, s);
        }
        c
    }

    fn run(&self, args: &[&str], speech: Option<&str>) -> Output {
        let out = self
            .cmd(args, speech)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_clean(&out);
        out
    }

    fn run_stdin(&self, args: &[&str], stdin: &str, speech: Option<&str>) -> Output {
        let mut child = self
            .cmd(args, speech)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_clean(&out);
        out
    }

    fn message(&mut self, ts: &str, text: &str, files: Value) {
        self.server
            .mock("POST", "/conversations.replies")
            .match_header("authorization", format!("Bearer {BOT}").as_str())
            .match_body(Matcher::AllOf(vec![
                Matcher::UrlEncoded("channel".into(), DM.into()),
                Matcher::UrlEncoded("ts".into(), ts.into()),
            ]))
            .with_body(
                json!({"ok": true, "has_more": false, "messages": [{
                    "type": "message", "subtype": "file_share", "user": "U00000002",
                    "text": text, "ts": ts, "files": files
                }]})
                .to_string(),
            )
            .create();
    }

    fn file_url(&self, path: &str) -> String {
        format!("{}/files-pri/{TEAM}-{path}", self.server.url())
    }

    fn serve(&mut self, path: &str, body: &[u8]) -> mockito::Mock {
        self.server
            .mock("GET", format!("/files-pri/{TEAM}-{path}").as_str())
            .match_header("authorization", format!("Bearer {BOT}").as_str())
            .with_header("content-type", "application/octet-stream")
            .with_body(body)
            .create()
    }
}

fn mock_install(server: &mut mockito::ServerGuard) {
    server
        .mock("POST", "/auth.test")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_header(
            "x-oauth-scopes",
            augmentagent_channel_slack::app::REQUIRED_BOT_SCOPES
                .join(",")
                .as_str(),
        )
        .with_body(
            json!({"ok": true, "team": "Example Test", "user": "jarvis", "team_id": TEAM,
                "user_id": "U00000001", "bot_id": "B00000001"})
            .to_string(),
        )
        .create();
    server
        .mock("POST", "/apps.connections.open")
        .with_body(
            json!({"ok": true, "url": "wss://wss.example.test/link/?ticket=t&app_id=A00000001"})
                .to_string(),
        )
        .create();
}

fn text(out: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_clean(out: &Output) {
    for stream in [&out.stdout, &out.stderr] {
        let s = String::from_utf8_lossy(stream);
        for secret in [APP, BOT] {
            assert!(!s.contains(secret), "{secret} leaked into output:\n{s}");
        }
    }
}

fn json_out(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", text(out)))
}

fn clip_json(id: &str, name: &str, mimetype: &str, size: usize, url: &str) -> Value {
    json!({"id": id, "name": name, "mimetype": mimetype, "size": size, "mode": "hosted",
        "subtype": "slack_audio", "media_display_type": "audio", "duration_ms": 1000,
        "is_external": false, "url_private": url, "url_private_download": url})
}

fn files_under(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn transcribe_prints_the_transcript_and_turn_text_and_cleans_up() {
    let mut env = Env::new();
    let wav = wav_1s();
    let clip = env.serve("F1/audio_message.webm", &wav);
    let url = env.file_url("F1/audio_message.webm");
    env.message(
        TS,
        "",
        json!([clip_json(
            "F1",
            "audio_message.webm",
            "audio/webm",
            wav.len(),
            &url
        )]),
    );

    let out = env.run(
        &[
            "slack",
            "voice",
            "transcribe",
            "--channel",
            DM,
            "--ts",
            TS,
            "--json",
        ],
        Some("ok:what is on my calendar"),
    );

    assert!(out.status.success(), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["fake_providers"], json!(true));
    assert_eq!(v["turn_text"], json!("what is on my calendar"));
    assert_eq!(v["transcripts"][0]["duration_ms"], json!(1000));
    assert_eq!(v["transcripts"][0]["provider"], json!("deepgram"));
    assert!(v["transcript_notice"]
        .as_str()
        .unwrap()
        .contains("what is on my calendar"));
    assert!(v["ffmpeg"].as_str().unwrap().ends_with("tools/ffmpeg"));
    assert_eq!(v["cleaned_up"], json!(true));
    clip.assert();
    assert!(files_under(&env.state().join("slack-inbound")).is_empty());
}

#[test]
fn transcribe_rejects_an_unsupported_codec_without_downloading() {
    let mut env = Env::new();
    let amr = env.serve("F1/voice.amr", b"#!AMR synthetic").expect(0);
    let url = env.file_url("F1/voice.amr");
    env.message(
        TS,
        "",
        json!([{"id": "F1", "name": "voice.amr", "mimetype": "audio/amr", "size": 15,
            "mode": "hosted", "url_private": url, "url_private_download": url}]),
    );

    let out = env.run(
        &[
            "slack",
            "voice",
            "transcribe",
            "--channel",
            DM,
            "--ts",
            TS,
            "--json",
        ],
        Some("ok"),
    );

    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["starts_turn"], json!(false));
    let detail = v["rejected"][0]["detail"].as_str().unwrap();
    assert!(
        detail.contains("unsupported audio format (audio/amr)"),
        "{detail}"
    );
    amr.assert();
}

#[test]
fn transcribe_reports_a_provider_failure() {
    let mut env = Env::new();
    let wav = wav_1s();
    env.serve("F1/audio_message.webm", &wav);
    let url = env.file_url("F1/audio_message.webm");
    env.message(
        TS,
        "",
        json!([clip_json(
            "F1",
            "audio_message.webm",
            "audio/webm",
            wav.len(),
            &url
        )]),
    );

    let out = env.run(
        &["slack", "voice", "transcribe", "--channel", DM, "--ts", TS],
        Some("fail:500"),
    );

    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("couldn't transcribe: deepgram failed (HTTP 500)"),
        "{s}"
    );
    assert!(s.contains("starts a turn: false"), "{s}");
}

#[derive(Default)]
struct Seen {
    posts: Mutex<Vec<Value>>,
    uploads: Mutex<Vec<Vec<u8>>>,
}

fn mock_delivery(env: &mut Env, seen: &Arc<Seen>) {
    let s = Arc::clone(seen);
    env.server
        .mock("POST", "/chat.postMessage")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let mut posts = s.posts.lock().unwrap();
            posts.push(body.clone());
            json!({"ok": true, "channel": body["channel"], "ts": format!("1790000000.00000{}", posts.len())})
                .to_string()
                .into_bytes()
        })
        .create();
    let upload_url = format!("{}/upload/v1/test", env.server.url());
    env.server
        .mock("POST", "/files.getUploadURLExternal")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .match_body(Matcher::UrlEncoded(
            "filename".into(),
            "spoken-reply.wav".into(),
        ))
        .with_body(
            json!({"ok": true, "upload_url": upload_url, "file_id": "F00000009"}).to_string(),
        )
        .create();
    let s = Arc::clone(seen);
    env.server
        .mock("POST", "/upload/v1/test")
        .with_body_from_request(move |req| {
            s.uploads.lock().unwrap().push(req.body().unwrap().clone());
            b"OK".to_vec()
        })
        .create();
    env.server
        .mock("POST", "/files.completeUploadExternal")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_body(json!({"ok": true, "files": [{"id": "F00000009"}]}).to_string())
        .create();
}

#[test]
fn speak_uploads_audio_with_a_text_mirror_once() {
    let mut env = Env::new();
    let seen = Arc::new(Seen::default());
    mock_delivery(&mut env, &seen);
    let answer = env.root.join("answer ü.md");
    std::fs::write(&answer, "**Two** meetings tomorrow.").unwrap();
    let args = [
        "slack",
        "voice",
        "speak",
        "--channel",
        DM,
        "--thread",
        TS,
        "--text-file",
        answer.to_str().unwrap(),
        "--json",
    ];

    let first = env.run(&args, Some("ok"));
    assert!(first.status.success(), "{}", text(&first));
    let v = json_out(&first);
    assert_eq!(v["speech"]["status"], json!("synthesized"));
    assert_eq!(v["sent_now"], json!(2));
    assert_eq!(v["audio_released"], json!(true));

    let again = env.run(&args, Some("ok"));
    assert!(again.status.success(), "{}", text(&again));
    let v = json_out(&again);
    assert_eq!(v["speech"]["status"], json!("already_queued"));
    assert_eq!(v["newly_enqueued"], json!(0));
    assert_eq!(v["sent_now"], json!(0));

    let posts = seen.posts.lock().unwrap();
    assert_eq!(posts.len(), 1, "text mirror posted once");
    assert_eq!(posts[0]["thread_ts"], json!(TS));
    assert!(posts[0]["text"]
        .as_str()
        .unwrap()
        .contains("*Two* meetings"));
    let uploads = seen.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1, "audio uploaded once");
    assert_eq!(&uploads[0][..4], b"RIFF");
    assert!(files_under(&env.state().join("slack-voice-replies")).is_empty());
}

#[test]
fn speak_without_a_tts_provider_fails_clearly_and_sends_nothing() {
    let mut env = Env::new();
    let seen = Arc::new(Seen::default());
    mock_delivery(&mut env, &seen);

    let out = env.run_stdin(
        &[
            "slack",
            "voice",
            "speak",
            "--channel",
            DM,
            "--stdin",
            "--json",
        ],
        "hello",
        None,
    );

    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["error"], json!("tts_unavailable"));
    assert!(seen.posts.lock().unwrap().is_empty());
}
