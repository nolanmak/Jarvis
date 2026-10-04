//! #1297 — voice clips and spoken replies on the real Slack dispatch path:
//! Socket Mode → durable inbound log → owner gate → voice clip transcribed
//! (download, fake `ffmpeg`, scripted speech-to-text) → the shared harness
//! in the same conversation → durable outbox → Web API, plus the owner's
//! `voice on|off|status` reply mode (persisted per conversation) delivering
//! answers as an uploaded audio file and the full text mirror.
//!
//! In-memory duplex WebSocket, `RecordingSlackWebApi`, temporary store,
//! state directories and model-selection file, a fake `ffmpeg` shell script
//! and scripted speech providers. No network, no paid provider. Identifiers
//! and audio are synthetic.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::native_session::{Launch, CURRENT};
use augmentagent_channel_slack::commands::{slack_selection, SlackCommandDeps, SlackCommands};
use augmentagent_channel_slack::harness::SlackConversationHarness;
use augmentagent_channel_slack::inbound::InboundOptions;
use augmentagent_channel_slack::interactive::{
    SlackInteractiveSurface, SlackSurfaceConfig, SlackWorkspaceRuntime,
};
use augmentagent_channel_slack::owner::SlackBotIdentity;
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::socket::{
    AsyncIo, BoxedWebSocket, ConnectError, SocketConnector, SocketModeConfig,
};
use augmentagent_channel_slack::transport::web::{
    PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi,
};
use augmentagent_channel_slack::voice::audio::{wav_from_pcm16, STT_SAMPLE_RATE};
use augmentagent_channel_slack::voice::fake::{ScriptedStt, ScriptedTts, SttScript, TtsScript};
use augmentagent_channel_slack::voice::reply::SpokenReplyOptions;
use augmentagent_channel_slack::voice::speech::UnconfiguredStt;
use augmentagent_channel_slack::voice::{ClipLimits, SlackVoice, SttStack, TtsStack};
use augmentagent_docs::ConvertOptions;
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::Store;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const BOT_USER: &str = "U0000000B";
const DM: &str = "D00000001";
const STRANGER_DM: &str = "D00000002";
const CONTROL: &str = "C00000001";
const T0: i64 = 1_700_000_000_000;
const SAID: &str = "what is on my calendar today";

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

type ServerSocket = WebSocketStream<DuplexStream>;

struct DuplexConnector {
    servers: mpsc::UnboundedSender<ServerSocket>,
}

#[async_trait]
impl SocketConnector for DuplexConnector {
    async fn connect(&self, _cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError> {
        let (client_half, server_half) = tokio::io::duplex(256 * 1024);
        let server = WebSocketStream::from_raw_socket(server_half, Role::Server, None).await;
        if self.servers.send(server).is_err() {
            return Err(ConnectError::Transient("test stopped listening".into()));
        }
        let boxed: Box<dyn AsyncIo> = Box::new(client_half);
        Ok(
            WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(boxed), Role::Client, None)
                .await,
        )
    }
}

/// Joins the turn's native session like the CLI adapters and answers with
/// the first line of what it was asked and the session it ran in.
#[derive(Default)]
struct EchoAgent {
    prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl QueryHandler for EchoAgent {
    async fn answer(&self, _ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        self.prompts.lock().unwrap().push(question.to_string());
        let session = CURRENT.try_with(Arc::clone)?;
        let provider = session.provider();
        let mut lease = session.begin(provider)?;
        let id = match lease.launch() {
            Launch::Resume { id } => id,
            Launch::Create {
                requested_id: Some(id),
            } => id,
            Launch::Create { requested_id: None } => {
                format!("{}-{}", provider.name(), uuid::Uuid::new_v4())
            }
        };
        lease.observe(&id)?;
        lease.finish()?;
        let first = question.lines().next().unwrap_or_default();
        Ok(format!("You said: {first}. session={id}"))
    }
}

impl EchoAgent {
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

/// `ms` of a quiet tone, 16 kHz mono: what the fake `ffmpeg` passes through.
fn tone_wav(ms: u64) -> Vec<u8> {
    let n = (STT_SAMPLE_RATE as u64 * ms / 1000) as usize;
    let samples: Vec<i16> = (0..n)
        .map(|i| (((i as f32) / 16.0).sin() * 2000.0) as i16)
        .collect();
    wav_from_pcm16(&samples, STT_SAMPLE_RATE)
}

const FAKE_FFMPEG: &str = r#"in=""; out=""; prev=""
for a in "$@"; do
  if [ "$prev" = "-i" ]; then in="$a"; fi
  prev="$a"; out="$a"
done
cp "$in" "$out""#;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

struct Harness {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
    wiki: PathBuf,
    selection: PathBuf,
    tools: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        let ffmpeg = tools.join("ffmpeg");
        std::fs::write(&ffmpeg, format!("#!/bin/sh\n{FAKE_FFMPEG}\n")).unwrap();
        std::fs::set_permissions(&ffmpeg, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let ws = workspace();
        store
            .bind_surface_owner(&ws.owner(OWNER).unwrap(), T0)
            .unwrap();
        store
            .set_surface_control_conversation(
                &ws.conversation(DM, None).unwrap(),
                ControlConversationKind::Direct,
                T0,
            )
            .unwrap();
        store
            .set_surface_control_conversation(
                &ws.conversation(CONTROL, None).unwrap(),
                ControlConversationKind::Channel,
                T0,
            )
            .unwrap();
        let selection = dir.path().join("config").join("model-selection.json");
        Harness {
            store,
            web: Arc::new(RecordingSlackWebApi::default()),
            wiki,
            selection,
            tools,
            dir,
        }
    }

    fn replies_root(&self) -> PathBuf {
        self.dir
            .path()
            .join("state dir ü")
            .join("slack-voice-replies")
    }

    fn voice(&self, stt: SttStack, tts: Option<TtsStack>) -> SlackVoice {
        SlackVoice {
            stt,
            tts,
            tools: ConvertOptions {
                search_path: Some(self.tools.as_os_str().to_owned()),
                fallback_dirs: vec![],
                // Generous: a fresh script's first run can be slow on a
                // loaded macOS host (the fake finishes in milliseconds).
                timeout: Duration::from_secs(30),
                ..ConvertOptions::default()
            },
            limits: ClipLimits::default(),
            replies: SpokenReplyOptions::new(self.replies_root()),
        }
    }

    fn surface(
        &self,
        agent: Arc<EchoAgent>,
        voice: SlackVoice,
    ) -> (
        SlackInteractiveSurface,
        mpsc::UnboundedReceiver<ServerSocket>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let harness =
            SlackConversationHarness::new(Arc::clone(&self.store), agent, self.wiki.clone())
                .with_selection(slack_selection(self.selection.clone()));
        let mut deps = SlackCommandDeps::new(self.selection.clone());
        deps.model_ready = Arc::new(|_| Ok(()));
        deps.voice = Some(voice.readiness());
        let commands = SlackCommands::new(Arc::clone(&self.store), deps);
        let surface = SlackInteractiveSurface::new(
            Arc::clone(&self.store),
            vec![SlackWorkspaceRuntime {
                workspace: workspace(),
                web: Arc::clone(&self.web) as Arc<dyn SlackWebApi>,
                bot: SlackBotIdentity {
                    bot_user_id: Some(BOT_USER.into()),
                    bot_id: Some("B00000001".into()),
                    app_id: Some("A00000001".into()),
                },
            }],
            vec![Arc::new(DuplexConnector { servers: tx }) as Arc<dyn SocketConnector>],
            Arc::new(harness),
            SlackSurfaceConfig {
                idle_poll: Duration::from_millis(50),
                heartbeat: Duration::from_secs(3600),
                socket: SocketModeConfig {
                    backoff: BackoffConfig {
                        initial: Duration::from_millis(50),
                        max: Duration::from_millis(200),
                    },
                    ..SocketModeConfig::default()
                },
                inbound: Some(InboundOptions::new(
                    self.dir.path().join("state dir ü").join("slack-inbound"),
                )),
                voice: Some(voice),
                ..SlackSurfaceConfig::default()
            },
        )
        .with_commands(Arc::new(commands));
        (surface, rx)
    }

    fn posts(&self) -> Vec<PostMessage> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostMessage(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn answers(&self, channel: &str, thread: Option<&str>) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|p| p.channel == channel && p.thread_ts.as_deref() == thread)
            .map(|p| p.text)
            .collect()
    }

    /// (filename, channel, thread, bytes) of every upload.
    fn uploads(&self) -> Vec<(String, Option<String>, Option<String>, usize)> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::UploadFile {
                    filename,
                    channel,
                    thread_ts,
                    bytes,
                } => Some((filename, channel, thread_ts, bytes)),
                _ => None,
            })
            .collect()
    }

    fn downloads(&self) -> usize {
        self.web
            .calls()
            .iter()
            .filter(|c| matches!(c, RecordedCall::DownloadFile { .. }))
            .count()
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Generous: macOS scans a freshly written script (the fake `ffmpeg`) on its
/// first run, which can take seconds on a loaded host.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Nothing more happens for a moment (exactly-once checks).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

struct Server {
    ws: ServerSocket,
}

impl Server {
    async fn accept(servers: &mut mpsc::UnboundedReceiver<ServerSocket>) -> Self {
        let mut ws = tokio::time::timeout(Duration::from_secs(5), servers.recv())
            .await
            .expect("client connects")
            .expect("connector alive");
        ws.send(Message::Text(
            json!({"type": "hello", "connection_info": {"app_id": "A00000001"}, "num_connections": 1})
                .to_string(),
        ))
        .await
        .unwrap();
        Server { ws }
    }

    async fn deliver(&mut self, frame: &Value) {
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .unwrap();
        let want = frame["envelope_id"].as_str().unwrap().to_string();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("an ack in time")
                .expect("socket open")
                .unwrap();
            if let Message::Text(text) = next {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["envelope_id"].as_str() == Some(want.as_str()) {
                    return;
                }
            }
        }
    }
}

struct Running {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start(surface: SlackInteractiveSurface) -> Running {
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    Running {
        shutdown,
        task: tokio::spawn(async move { surface.run(sd).await }),
    }
}

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("surface stops promptly")
            .expect("no panic")
            .expect("clean stop");
    }
}

fn event(envelope_id: &str, event: Value) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "event_callback", "team_id": TEAM, "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"), "event_time": 1_700_000_000,
            "event": event,
        }
    })
}

fn owner_dm(envelope_id: &str, text: &str, ts: &str) -> Value {
    event(
        envelope_id,
        json!({"type": "message", "channel": DM, "channel_type": "im",
            "user": OWNER, "text": text, "ts": ts}),
    )
}

fn file_url(id: &str) -> String {
    format!("https://files.slack.com/files-pri/{TEAM}-{id}/download/audio_message.webm")
}

/// A Slack audio clip (the file object Slack documents) from `user` in
/// `channel`, with no typed text.
fn clip_message(envelope_id: &str, user: &str, channel: &str, ts: &str, file_id: &str) -> Value {
    let channel_type = if channel.starts_with('D') {
        "im"
    } else {
        "channel"
    };
    event(
        envelope_id,
        json!({
            "type": "message", "subtype": "file_share", "channel": channel,
            "channel_type": channel_type, "user": user, "text": "", "ts": ts,
            "files": [{
                "id": file_id, "name": "audio_message.webm", "title": "Audio clip",
                "mimetype": "audio/webm", "filetype": "webm", "subtype": "slack_audio",
                "media_display_type": "audio", "duration_ms": 1000, "size": 32044,
                "mode": "hosted", "url_private": file_url(file_id),
                "url_private_download": file_url(file_id),
            }]
        }),
    )
}

fn session_of(answer: &str) -> String {
    answer
        .split_whitespace()
        .find_map(|w| w.strip_prefix("session="))
        .unwrap_or_else(|| panic!("no session in {answer:?}"))
        .to_string()
}

fn stt_says(text: &str) -> ScriptedStt {
    ScriptedStt::new("whisper-cpp", vec![SttScript::Text(text.into())])
}

// ---------------------------------------------------------------------------
// Voice clips in
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_clip_is_a_turn_in_the_same_conversation_with_the_transcript_shown() {
    let h = Harness::new();
    h.web.add_file(&file_url("F00000001"), tone_wav(1_000));
    let stt = stt_says(SAID);
    let agent = Arc::new(EchoAgent::default());
    let (surface, mut servers) = h.surface(
        agent.clone(),
        h.voice(SttStack::new(Arc::new(stt.clone())), None),
    );
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&clip_message(
            "e1",
            OWNER,
            DM,
            "1700000001.000100",
            "F00000001",
        ))
        .await;
    eventually("transcript and answer", || h.answers(DM, None).len() == 2).await;

    let answers = h.answers(DM, None);
    // The transcript notice comes first, then the answer to what was said.
    assert!(answers[0].contains("Transcript"), "{}", answers[0]);
    assert!(answers[0].contains(SAID), "{}", answers[0]);
    assert!(answers[0].contains("0:01"), "{}", answers[0]);
    assert!(
        answers[1].starts_with(&format!("You said: {SAID}.")),
        "{}",
        answers[1]
    );
    assert_eq!(agent.prompts(), vec![SAID.to_string()]);
    assert_eq!(stt.seen().len(), 1);
    assert_eq!(stt.seen()[0].sample_rate, STT_SAMPLE_RATE);

    // A typed follow-up continues the same native session.
    server
        .deliver(&owner_dm("e2", "and tomorrow?", "1700000002.000100"))
        .await;
    eventually("follow-up", || h.answers(DM, None).len() == 3).await;
    let follow = h.answers(DM, None)[2].clone();
    assert_eq!(session_of(&follow), session_of(&answers[1]));

    // The clip never outlives its turn.
    let inbound = h.dir.path().join("state dir ü").join("slack-inbound");
    eventually("clip removed", || entries(&inbound).is_empty()).await;
    settle().await;
    assert_eq!(h.answers(DM, None).len(), 3, "each posted once");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_speech_to_text_a_clip_gets_a_clear_notice_and_no_turn() {
    let h = Harness::new();
    h.web.add_file(&file_url("F00000001"), tone_wav(1_000));
    let stt = UnconfiguredStt::new("whisper-cpp", "whisper.cpp is not installed on this host");
    let agent = Arc::new(EchoAgent::default());
    let (surface, mut servers) =
        h.surface(agent.clone(), h.voice(SttStack::new(Arc::new(stt)), None));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&clip_message(
            "e1",
            OWNER,
            DM,
            "1700000001.000100",
            "F00000001",
        ))
        .await;
    eventually("notice", || h.answers(DM, None).len() == 1).await;
    let notice = h.answers(DM, None)[0].clone();
    assert!(notice.contains("audio_message.webm"), "{notice}");
    assert!(notice.contains("can't be transcribed"), "{notice}");
    assert!(notice.contains("whisper.cpp is not installed"), "{notice}");
    settle().await;
    assert!(agent.prompts().is_empty(), "no turn ran");
    assert_eq!(h.downloads(), 0, "nothing was downloaded");
    assert_eq!(h.answers(DM, None).len(), 1);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_owner_clip_never_reaches_speech_to_text() {
    let h = Harness::new();
    h.web.add_file(&file_url("F00000009"), tone_wav(1_000));
    let stt = stt_says("should never be heard");
    let agent = Arc::new(EchoAgent::default());
    let (surface, mut servers) = h.surface(
        agent.clone(),
        h.voice(SttStack::new(Arc::new(stt.clone())), None),
    );
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&clip_message(
            "e1",
            STRANGER,
            STRANGER_DM,
            "1700000001.000100",
            "F00000009",
        ))
        .await;
    server
        .deliver(&clip_message(
            "e2",
            STRANGER,
            CONTROL,
            "1700000002.000100",
            "F00000009",
        ))
        .await;
    // The DM rejection is posted; then nothing else happens.
    eventually("rejection", || !h.answers(STRANGER_DM, None).is_empty()).await;
    settle().await;
    assert!(stt.seen().is_empty(), "zero speech-to-text calls");
    assert_eq!(h.downloads(), 0, "non-owner audio is never downloaded");
    assert!(agent.prompts().is_empty());
    assert!(h.uploads().is_empty());
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Spoken replies out
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_on_answers_with_audio_plus_the_text_once_and_voice_off_stops_it() {
    let h = Harness::new();
    let tts = ScriptedTts::new("deepgram", vec![TtsScript::Pcm24k(400)]);
    let agent = Arc::new(EchoAgent::default());
    let (surface, mut servers) = h.surface(
        agent.clone(),
        h.voice(
            SttStack::new(Arc::new(stt_says(SAID))),
            Some(TtsStack::new(Arc::new(tts.clone()))),
        ),
    );
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&owner_dm("e1", "voice on", "1700000001.000100"))
        .await;
    eventually("voice on", || h.answers(DM, None).len() == 1).await;
    let on = h.answers(DM, None)[0].clone();
    assert!(on.contains("Spoken replies are on"), "{on}");
    assert!(agent.prompts().is_empty(), "a command, not a turn");

    server
        .deliver(&owner_dm("e2", "hello", "1700000002.000100"))
        .await;
    eventually("spoken answer", || {
        h.answers(DM, None).len() == 2 && h.uploads().len() == 1
    })
    .await;
    let text = h.answers(DM, None)[1].clone();
    assert!(
        text.starts_with("You said: hello."),
        "full text mirror: {text}"
    );
    let (name, channel, thread, bytes) = h.uploads()[0].clone();
    assert_eq!(name, "spoken-reply.wav");
    assert_eq!((channel.as_deref(), thread.as_deref()), (Some(DM), None));
    assert!(bytes > 44, "a real WAV");
    assert_eq!(tts.seen().len(), 1);
    assert!(
        tts.seen()[0].starts_with("You said: hello."),
        "{:?}",
        tts.seen()
    );
    // The stored audio goes once the upload is settled.
    let root = h.replies_root();
    eventually("audio released", || entries(&root).is_empty()).await;
    settle().await;
    assert_eq!(h.uploads().len(), 1, "audio delivered once");
    assert_eq!(h.answers(DM, None).len(), 2, "text delivered once");

    server
        .deliver(&owner_dm("e3", "voice off", "1700000003.000100"))
        .await;
    eventually("voice off", || h.answers(DM, None).len() == 3).await;
    assert!(
        h.answers(DM, None)[2].contains("off"),
        "{}",
        h.answers(DM, None)[2]
    );
    server
        .deliver(&owner_dm("e4", "hello again", "1700000004.000100"))
        .await;
    eventually("text answer", || h.answers(DM, None).len() == 4).await;
    settle().await;
    assert_eq!(h.uploads().len(), 1, "no audio after voice off");
    assert_eq!(tts.seen().len(), 1);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tts_failure_still_delivers_the_text_with_a_note() {
    let h = Harness::new();
    let tts = ScriptedTts::new("deepgram", vec![TtsScript::Fail("500".into())]);
    let agent = Arc::new(EchoAgent::default());
    let (surface, mut servers) = h.surface(
        agent.clone(),
        h.voice(
            SttStack::new(Arc::new(stt_says(SAID))),
            Some(TtsStack::new(Arc::new(tts.clone()))),
        ),
    );
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&owner_dm("e1", "!voice on", "1700000001.000100"))
        .await;
    eventually("voice on", || h.answers(DM, None).len() == 1).await;
    server
        .deliver(&owner_dm("e2", "hello", "1700000002.000100"))
        .await;
    eventually("answer", || h.answers(DM, None).len() == 2).await;
    let text = h.answers(DM, None)[1].clone();
    assert!(text.starts_with("You said: hello."), "{text}");
    assert!(text.contains("Spoken reply unavailable"), "{text}");
    assert!(text.contains("deepgram failed (HTTP 500)"), "{text}");
    settle().await;
    assert!(h.uploads().is_empty());
    assert_eq!(tts.seen().len(), 1);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reply_mode_survives_a_restart_and_a_dm_thread_inherits_it() {
    let h = Harness::new();
    let tts = ScriptedTts::new("deepgram", vec![TtsScript::Wav(300)]);
    let voice = || {
        h.voice(
            SttStack::new(Arc::new(stt_says(SAID))),
            Some(TtsStack::new(Arc::new(tts.clone()))),
        )
    };
    let agent = Arc::new(EchoAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), voice());
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&owner_dm("e1", "voice on", "1700000001.000100"))
        .await;
    eventually("voice on", || h.answers(DM, None).len() == 1).await;
    running.stop().await;

    // Restart: a new surface over the same store.
    let (surface, mut servers) = h.surface(agent.clone(), voice());
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&owner_dm("e2", "after restart", "1700000002.000100"))
        .await;
    eventually("spoken after restart", || h.uploads().len() == 1).await;

    // A thread in the DM has no choice of its own: it inherits the DM's.
    const PARENT: &str = "1700000002.000100";
    server
        .deliver(&event(
            "e3",
            json!({"type": "message", "channel": DM, "channel_type": "im", "user": OWNER,
                "text": "in a thread", "ts": "1700000003.000100", "thread_ts": PARENT}),
        ))
        .await;
    eventually("spoken in the thread", || h.uploads().len() == 2).await;
    assert_eq!(h.uploads()[1].2.as_deref(), Some(PARENT));

    server
        .deliver(&owner_dm("e4", "voice status", "1700000004.000100"))
        .await;
    eventually("status", || {
        h.answers(DM, None)
            .iter()
            .any(|a| a.contains("Spoken replies: on"))
    })
    .await;
    let status = h
        .answers(DM, None)
        .into_iter()
        .find(|a| a.contains("Spoken replies: on"))
        .unwrap();
    assert!(status.contains("deepgram"), "{status}");
    assert!(status.contains("whisper-cpp"), "{status}");
    assert!(
        status.contains("#1298"),
        "live voice stays a named blocker: {status}"
    );
    running.stop().await;
}
