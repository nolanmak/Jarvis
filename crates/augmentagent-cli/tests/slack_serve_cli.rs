//! #1287 — `augmentagent serve` runs the interactive Slack surface with only
//! Slack configured: no Discord token, IDs or state, no WhatsApp state, no
//! Composio key. The real binary talks to a local fake Slack: mockito for
//! the Web API (`AUGMENTAGENT_SLACK_API_BASE`) and an in-test WebSocket
//! server for Socket Mode. The app is installed and the owner bound through
//! the real CLI, credentials live in the plaintext test store
//! (`AUGMENTAGENT_INSECURE_CREDENTIAL_DIR`), and turns run the real
//! conversation harness with the debug-build fake agent
//! (`AUGMENTAGENT_TEST_SLACK_TURN_REPLY`), so no reasoner or provider is
//! ever called. Tokens and IDs are synthetic.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use mockito::Matcher;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const APP: &str = "xapp-test-000";
const BOT: &str = "xoxb-test-000";
const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const OWNER_DM: &str = "D00000001";
const STRANGER_DM: &str = "D00000002";
const REJECTION: &str = "Sorry, I only take requests from my owner here.";
const SCOPES: &str = "app_mentions:read,channels:history,channels:read,chat:write,commands,\
files:read,files:write,groups:history,groups:read,im:history,im:read,im:write,mpim:history,mpim:read,\
reactions:write,users:read";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    api: String,
}

impl Env {
    fn new(api: String) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Spaces and non-ASCII in every path the daemon touches.
        let root = tmp.path().join("state dir ü");
        std::fs::create_dir_all(root.join("home")).unwrap();
        Env {
            _tmp: tmp,
            root,
            api,
        }
    }

    fn db(&self) -> PathBuf {
        self.root.join("agent.db")
    }

    /// Nothing from the developer's shell leaks in: no Discord, WhatsApp,
    /// Composio or provider settings, no real Keychain.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        c.current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg state"))
            .env("AUGMENTAGENT_DB", self.db())
            .env(
                "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
                self.root.join("creds"),
            )
            .env("AUGMENTAGENT_SLACK_API_BASE", &self.api)
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env(
                "AUGMENTAGENT_COOLDOWN_FILE",
                self.root.join("cooldowns.json"),
            )
            // Belt and braces: even without the stub nothing could run.
            .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
            .env("CLAUDE_CLI", self.root.join("no-such-claude"))
            .env("AUGMENTAGENT_TEST_SLACK_TURN_REPLY", "STUB:")
            .env("DASHBOARD_PORT", "1")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "info")
            .args(args);
        c
    }

    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .cmd(args)
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
        assert!(
            out.status.success(),
            "{args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// `slack app install` + `slack app owner bind`, through the real CLI.
    fn install_and_bind(&self) {
        self.run(
            &["slack", "app", "install", "--stdin"],
            &format!("{APP}\n{BOT}\n"),
        );
        self.run(&["slack", "app", "owner", "bind", "--user", OWNER], "");
    }

    fn serve(&self, extra: &[&str]) -> Child {
        self.serve_with(extra, &[])
    }

    fn serve_with(&self, extra: &[&str], env: &[(&str, &str)]) -> Child {
        let mut args = vec!["serve", "--no-email", "true"];
        args.extend_from_slice(extra);
        self.cmd(&args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// `status --json`. A second process opening the database while the
    /// daemon is mid-migration can see `database is locked` (Store::open
    /// switches to WAL before its busy timeout applies), so retry briefly.
    fn status(&self) -> Value {
        let mut last = String::new();
        for _ in 0..20 {
            let out = self
                .cmd(&["status", "--json", "true"])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            match serde_json::from_slice(&out.stdout) {
                Ok(v) => return v,
                Err(e) => last = format!("{e}: {}", String::from_utf8_lossy(&out.stderr)),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("status JSON never parsed: {last}");
    }

    fn slack_status(&self) -> Value {
        self.status()["interactive"]["slack"].clone()
    }

    fn outbox(&self) -> Vec<(String, String, Option<String>)> {
        let conn = rusqlite::Connection::open(self.db()).unwrap();
        let mut stmt = conn
            .prepare("SELECT idempotency_key, status, provider_message_id FROM surface_outbox ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }
}

fn interrupt(child: &Child) {
    let ok = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -INT failed");
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn logs(mut child: Child) -> String {
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

async fn eventually(what: &str, within: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// A one-connection-at-a-time fake Socket Mode endpoint. Frames pushed on
/// `send` go to the connected client; acks come back on `acks`; dropping
/// `kill` closes the socket and stops accepting, like an outage.
struct FakeSocket {
    port: u16,
    send: mpsc::UnboundedSender<Value>,
    acks: mpsc::UnboundedReceiver<String>,
    kill: Option<tokio::sync::oneshot::Sender<()>>,
}

async fn fake_socket() -> FakeSocket {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Value>();
    let (ack_tx, ack_rx) = mpsc::unbounded_channel::<String>();
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = tokio::select! {
                _ = &mut kill_rx => return,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            let hello = json!({"type": "hello", "num_connections": 1,
                "connection_info": {"app_id": "A00000001"}});
            ws.send(Message::Text(hello.to_string())).await.unwrap();
            loop {
                tokio::select! {
                    _ = &mut kill_rx => {
                        let _ = ws.close(None).await;
                        return;
                    }
                    frame = send_rx.recv() => match frame {
                        Some(f) => ws.send(Message::Text(f.to_string())).await.unwrap(),
                        None => return,
                    },
                    incoming = ws.next() => match incoming {
                        Some(Ok(Message::Text(t))) => {
                            let v: Value = serde_json::from_str(&t).unwrap();
                            if let Some(id) = v["envelope_id"].as_str() {
                                let _ = ack_tx.send(id.to_string());
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                }
            }
        }
    });
    FakeSocket {
        port,
        send: send_tx,
        acks: ack_rx,
        kill: Some(kill_tx),
    }
}

impl FakeSocket {
    async fn ack(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(10), self.acks.recv())
            .await
            .expect("ack in time")
            .expect("socket alive")
    }
}

fn dm(envelope_id: &str, channel: &str, user: &str, text: &str, ts: &str) -> Value {
    json!({
        "type": "events_api", "envelope_id": envelope_id, "accepts_response_payload": false,
        "payload": {
            "type": "event_callback", "team_id": TEAM, "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"), "event_time": 1_800_000_000,
            "event": {"type": "message", "channel": channel, "channel_type": "im",
                "user": user, "text": text, "ts": ts}
        }
    })
}

async fn mock_slack(server: &mut mockito::ServerGuard, ws_port: u16) {
    server
        .mock("POST", "/auth.test")
        .with_header("x-oauth-scopes", SCOPES)
        .with_body(
            json!({"ok": true, "team": "Example Test", "team_id": TEAM, "user": "jarvis",
                "user_id": "U0000000B", "bot_id": "B00000001", "app_id": "A00000001"})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("POST", "/apps.connections.open")
        .with_body(
            json!({"ok": true,
                "url": format!("ws://127.0.0.1:{ws_port}/link/?ticket=ticket-test-000&app_id=A00000001")})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("POST", "/users.info")
        .with_body(
            json!({"ok": true, "user": {"id": OWNER, "team_id": TEAM, "name": "owner",
                "real_name": "Owner Example", "is_bot": false, "deleted": false}})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("POST", "/conversations.open")
        .with_body(json!({"ok": true, "channel": {"id": OWNER_DM}}).to_string())
        .create_async()
        .await;
}

fn assert_no_tokens(text: &str) {
    for t in [APP, BOT, "ticket-test-000"] {
        assert!(!text.contains(t), "secret {t} in output");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slack_only_serve_answers_the_owner_rejects_a_stranger_and_reports_live_health() {
    let mut socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let answer = server
        .mock("POST", "/chat.postMessage")
        .match_body(Matcher::AllOf(vec![
            Matcher::PartialJson(json!({"channel": OWNER_DM})),
            // #1288 — the fake agent runs inside the harness's native session.
            Matcher::Regex(
                r#""text":"STUB: what is due today\? \(session [0-9a-f-]+, turn 1\)""#.into(),
            ),
        ]))
        .with_body(json!({"ok": true, "channel": OWNER_DM, "ts": "1800000000.000001"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let rejection = server
        .mock("POST", "/chat.postMessage")
        .match_body(Matcher::PartialJson(
            json!({"channel": STRANGER_DM, "text": REJECTION}),
        ))
        .with_body(
            json!({"ok": true, "channel": STRANGER_DM, "ts": "1800000000.000002"}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;

    let env = Env::new(server.url());
    env.install_and_bind();
    assert_eq!(
        env.slack_status()["state"],
        json!("disconnected"),
        "bound, daemon not running"
    );

    let mut serve = env.serve(&["--dry-run", "false"]);
    eventually("connected", Duration::from_secs(30), || {
        env.slack_status()["state"] == json!("connected")
    })
    .await;
    let s = env.slack_status();
    assert_eq!(s["healthy"], json!(true));
    assert_eq!(s["workspaces"], json!([TEAM]));
    assert_eq!(s["last_event_unix"], Value::Null);

    socket
        .send
        .send(dm(
            "env-1",
            OWNER_DM,
            OWNER,
            "what is due today?",
            "1800000001.000100",
        ))
        .unwrap();
    assert_eq!(socket.ack().await, "env-1");
    socket
        .send
        .send(dm(
            "env-2",
            STRANGER_DM,
            STRANGER,
            "give me the owner's mail",
            "1800000001.000200",
        ))
        .unwrap();
    assert_eq!(socket.ack().await, "env-2");
    // Slack redelivers the owner's message: acked, never a second turn.
    let mut again = dm(
        "env-3",
        OWNER_DM,
        OWNER,
        "what is due today?",
        "1800000001.000100",
    );
    again["retry_attempt"] = json!(1);
    socket.send.send(again).unwrap();
    assert_eq!(socket.ack().await, "env-3");

    eventually(
        "answer and rejection posted",
        Duration::from_secs(15),
        || answer.matched() && rejection.matched(),
    )
    .await;
    eventually("last send reported", Duration::from_secs(10), || {
        env.slack_status()["last_send_unix"].is_i64()
    })
    .await;
    let s = env.slack_status();
    assert_eq!(s["state"], json!("connected"));
    assert!(s["last_event_unix"].is_i64(), "{s}");

    // Outage: the socket dies and Slack refuses new ones.
    drop(socket.kill.take());
    eventually("reconnecting", Duration::from_secs(15), || {
        let s = env.slack_status();
        s["state"] == json!("reconnecting") && s["healthy"] == json!(false)
    })
    .await;
    assert!(env.slack_status()["recovery"].is_string());

    interrupt(&serve);
    let status = wait_exit(&mut serve, Duration::from_secs(20));
    let out = logs(serve);
    let status = status.unwrap_or_else(|| panic!("serve did not exit on SIGINT:\n{out}"));
    assert!(status.success(), "serve exited {status}:\n{out}");
    assert_eq!(env.slack_status()["state"], json!("stopped"));
    assert_no_tokens(&out);
    assert!(out.contains("slack interactive: stopped"), "{out}");
    assert!(!out.contains("discord approval broker disabled"), "{out}");

    answer.assert_async().await;
    rejection.assert_async().await;
    let rows = env.outbox();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter().all(|(_, status, _)| status == "sent"),
        "{rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_serve_makes_zero_live_sends_and_exits_cleanly_on_sigint() {
    let mut socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let any_send = server
        .mock("POST", Matcher::Regex(r"^/chat\.".into()))
        .with_body(json!({"ok": true, "channel": OWNER_DM, "ts": "1"}).to_string())
        .expect(0)
        .create_async()
        .await;

    let env = Env::new(server.url());
    env.install_and_bind();
    // `serve` is dry-run unless told otherwise.
    let mut serve = env.serve(&[]);
    eventually("connected", Duration::from_secs(30), || {
        env.slack_status()["state"] == json!("connected")
    })
    .await;
    assert_eq!(env.slack_status()["dry_run"], json!(true));

    socket
        .send
        .send(dm(
            "env-1",
            OWNER_DM,
            OWNER,
            "dry question",
            "1800000002.000100",
        ))
        .unwrap();
    socket.ack().await;
    socket
        .send
        .send(dm(
            "env-2",
            STRANGER_DM,
            STRANGER,
            "hello",
            "1800000002.000200",
        ))
        .unwrap();
    socket.ack().await;
    eventually("both sends recorded", Duration::from_secs(15), || {
        let rows = env.outbox();
        rows.len() == 2 && rows.iter().all(|(_, s, _)| s == "sent")
    })
    .await;
    for (key, _, provider) in env.outbox() {
        assert!(
            provider
                .as_deref()
                .is_some_and(|p| p.starts_with("dry-run:")),
            "{key}: {provider:?}"
        );
    }

    interrupt(&serve);
    let status = wait_exit(&mut serve, Duration::from_secs(20));
    let out = logs(serve);
    let status = status.unwrap_or_else(|| panic!("dry-run serve did not exit:\n{out}"));
    assert!(status.success(), "serve exited {status}:\n{out}");
    assert_no_tokens(&out);
    assert_eq!(env.slack_status()["state"], json!("stopped"));
    any_send.assert_async().await;
}

/// Slack failing (not set up, or forced on without an install) never stops
/// `serve`: the surface reports why and how to fix it.
fn slack_inactive_keeps_serve_running(switch: Option<&str>, state: &str, detail: &str) {
    let env = Env::new("http://127.0.0.1:9".into());
    let extra: Vec<(&str, &str)> = switch
        .map(|v| vec![("AUGMENTAGENT_SLACK_INTERACTIVE", v)])
        .unwrap_or_default();
    let mut serve = env.serve_with(&[], &extra);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut report = Value::Null;
    while Instant::now() < deadline {
        report = env.slack_status();
        // The daemon's own report (it has a heartbeat), not status's
        // fallback for "nothing reported".
        if report["state"] == json!(state) && report["heartbeat_unix"].is_i64() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let still_running = serve.try_wait().unwrap().is_none();
    if still_running {
        interrupt(&serve);
    }
    let exit = wait_exit(&mut serve, Duration::from_secs(20));
    let out = logs(serve);
    assert_eq!(report["state"], json!(state), "{report}\n{out}");
    assert_eq!(report["detail"], json!(detail), "{report}");
    assert_eq!(report["healthy"], json!(false));
    assert!(
        report["recovery"]
            .as_str()
            .unwrap_or("")
            .contains("slack app install"),
        "{report}"
    );
    assert!(
        still_running,
        "serve must not exit because Slack is not usable:\n{out}"
    );
    assert!(
        exit.is_some_and(|s| s.success()),
        "serve did not stop cleanly:\n{out}"
    );
}

#[test]
fn serve_without_any_slack_app_reports_not_configured_and_keeps_running() {
    slack_inactive_keeps_serve_running(
        None,
        "not_configured",
        "no interactive Slack app is installed",
    );
}

#[test]
fn slack_forced_on_without_an_install_is_misconfigured_and_serve_keeps_running() {
    slack_inactive_keeps_serve_running(
        Some("on"),
        "misconfigured",
        "no interactive Slack app is installed",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slack_keeps_serving_when_discord_and_whatsapp_fail_to_start() {
    let mut socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let answer = server
        .mock("POST", "/chat.postMessage")
        .match_body(Matcher::AllOf(vec![
            Matcher::PartialJson(json!({"channel": OWNER_DM})),
            Matcher::Regex(r#""text":"STUB: still here\? \(session "#.into()),
        ]))
        .with_body(json!({"ok": true, "channel": OWNER_DM, "ts": "1800000000.000001"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let env = Env::new(server.url());
    env.install_and_bind();
    let missing = env.root.join("no such whatsapp export");
    // Discord: a token without the channel ID its broker requires (fails
    // before any connection). WhatsApp: an export directory that is absent.
    let mut serve = env.serve_with(
        &["--dry-run", "false"],
        &[
            ("DISCORD_BOT_TOKEN", "discord-test-not-a-token"),
            (
                "AUGMENTAGENT_WHATSAPP_HISTORY_DIR",
                missing.to_str().unwrap(),
            ),
        ],
    );
    eventually("connected", Duration::from_secs(30), || {
        env.slack_status()["state"] == json!("connected")
    })
    .await;
    socket
        .send
        .send(dm(
            "env-1",
            OWNER_DM,
            OWNER,
            "still here?",
            "1800000003.000100",
        ))
        .unwrap();
    socket.ack().await;
    eventually("answered", Duration::from_secs(15), || answer.matched()).await;

    interrupt(&serve);
    let exit = wait_exit(&mut serve, Duration::from_secs(20));
    let out = logs(serve);
    assert!(
        exit.is_some_and(|s| s.success()),
        "serve did not stop cleanly:\n{out}"
    );
    assert!(out.contains("discord approval broker disabled"), "{out}");
    assert!(out.contains("WhatsApp history disabled"), "{out}");
    assert!(
        !out.contains("discord-test-not-a-token"),
        "token logged:\n{out}"
    );
    answer.assert_async().await;
}

fn owner_event(envelope_id: &str, event: Value) -> Value {
    json!({
        "type": "events_api", "envelope_id": envelope_id, "accepts_response_payload": false,
        "payload": {
            "type": "event_callback", "team_id": TEAM, "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"), "event_time": 1_800_000_000,
            "event": event
        }
    })
}

/// `(session <id>, turn <n>)` from a fake-agent answer.
fn session_turn(text: &str) -> Option<(String, u64)> {
    let rest = text.split("(session ").nth(1)?;
    let (id, rest) = rest.split_once(", turn ")?;
    let n = rest.split(')').next()?.parse().ok()?;
    Some((id.to_string(), n))
}

// #1288 — the built `serve` runs owner turns through the shared harness: one
// native session per Slack conversation that follow-ups resume, a DM thread
// is its own conversation, `cancel` stops a running turn at once and says so
// (the status line tells the owner how), and an owner file reaches the
// agent readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_runs_owner_turns_through_the_harness_with_sessions_cancel_and_files() {
    use std::sync::{Arc, Mutex};
    let mut socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let posts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorded = Arc::clone(&posts);
    server
        .mock("POST", "/chat.postMessage")
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            let mut all = recorded.lock().unwrap();
            all.push(body.clone());
            json!({"ok": true, "channel": body["channel"],
                "ts": format!("1800000100.{:06}", all.len())})
            .to_string()
            .into()
        })
        .expect_at_least(1)
        .create_async()
        .await;
    server
        .mock("POST", "/chat.update")
        .with_body_from_request(|req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            json!({"ok": true, "channel": body["channel"], "ts": body["ts"]})
                .to_string()
                .into()
        })
        .create_async()
        .await;
    server
        .mock("GET", "/files-pri/T00000001-F00000001/download/notes.txt")
        .with_header("content-type", "text/plain")
        .with_body("synthetic notes body\n")
        .create_async()
        .await;
    let file_host = server.host_with_port();
    let texts = |channel: &str, thread: Option<&str>| -> Vec<String> {
        posts
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p["channel"] == json!(channel) && p["thread_ts"].as_str() == thread)
            .filter_map(|p| p["text"].as_str().map(str::to_string))
            .collect()
    };

    let env = Env::new(server.url());
    env.install_and_bind();
    let mut serve = env.serve_with(
        &["--dry-run", "false"],
        &[("AUGMENTAGENT_SLACK_TEST_FILE_HOSTS", file_host.as_str())],
    );
    eventually("connected", Duration::from_secs(30), || {
        env.slack_status()["state"] == json!("connected")
    })
    .await;
    let dm_msg = |id: &str, text: &str, ts: &str| {
        owner_event(
            id,
            json!({"type": "message", "channel": OWNER_DM, "channel_type": "im",
            "user": OWNER, "text": text, "ts": ts}),
        )
    };
    let answered = |channel: &str, thread: Option<&str>, n: usize| {
        texts(channel, thread)
            .iter()
            .filter(|t| t.starts_with("STUB:"))
            .count()
            >= n
    };

    socket
        .send
        .send(dm_msg("h1", "first question", "1800000004.000100"))
        .unwrap();
    socket.ack().await;
    eventually("first answer", Duration::from_secs(15), || {
        answered(OWNER_DM, None, 1)
    })
    .await;
    socket
        .send
        .send(dm_msg("h2", "second question", "1800000004.000200"))
        .unwrap();
    socket.ack().await;
    eventually("second answer", Duration::from_secs(15), || {
        answered(OWNER_DM, None, 2)
    })
    .await;
    let thread = owner_event(
        "h3",
        json!({"type": "message", "channel": OWNER_DM,
        "channel_type": "im", "user": OWNER, "text": "side topic",
        "ts": "1800000004.000300", "thread_ts": "1800000004.000100"}),
    );
    socket.send.send(thread).unwrap();
    socket.ack().await;
    eventually("thread answer", Duration::from_secs(15), || {
        answered(OWNER_DM, Some("1800000004.000100"), 1)
    })
    .await;
    let dm: Vec<(String, u64)> = texts(OWNER_DM, None)
        .iter()
        .filter_map(|t| session_turn(t))
        .collect();
    let side = session_turn(&texts(OWNER_DM, Some("1800000004.000100"))[1]).unwrap();
    assert_eq!(dm.len(), 2, "{dm:?}");
    assert_eq!(dm[0].0, dm[1].0, "the DM is one native session");
    assert_eq!((dm[0].1, dm[1].1), (1, 2));
    assert_ne!(side.0, dm[0].0, "a DM thread is its own session");
    assert_eq!(side.1, 1);
    assert!(
        texts(OWNER_DM, None)
            .iter()
            .any(|t| t.contains("reply `cancel` to stop")),
        "the status line says how to cancel: {:?}",
        texts(OWNER_DM, None)
    );

    // A slow turn, then `cancel`: stopped at once and reported.
    socket
        .send
        .send(dm_msg("h4", "slow job", "1800000004.000400"))
        .unwrap();
    socket.ack().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let sent = Instant::now();
    socket
        .send
        .send(dm_msg("h5", "cancel", "1800000004.000500"))
        .unwrap();
    socket.ack().await;
    eventually("cancel reported", Duration::from_secs(10), || {
        texts(OWNER_DM, None)
            .iter()
            .any(|t| t.starts_with("Stopped."))
    })
    .await;
    assert!(
        sent.elapsed() < Duration::from_secs(10),
        "cancel waited for the turn"
    );

    // An owner file reaches the agent, which can open it.
    let file = owner_event(
        "h6",
        json!({"type": "message", "subtype": "file_share",
        "channel": OWNER_DM, "channel_type": "im", "user": OWNER, "text": "summarize this",
        "ts": "1800000004.000600",
        "files": [{"id": "F00000001", "name": "notes.txt", "mimetype": "text/plain",
            "size": 21, "mode": "hosted",
            "url_private_download": format!("{}/files-pri/T00000001-F00000001/download/notes.txt", server.url())}]}),
    );
    socket.send.send(file).unwrap();
    socket.ack().await;
    eventually("file acknowledged", Duration::from_secs(15), || {
        texts(OWNER_DM, None)
            .iter()
            .any(|t| t.contains("read 00-notes.txt (21 bytes)"))
    })
    .await;

    interrupt(&serve);
    let status = wait_exit(&mut serve, Duration::from_secs(20));
    let out = logs(serve);
    assert!(
        status.is_some_and(|s| s.success()),
        "serve did not stop cleanly:\n{out}"
    );
    assert_no_tokens(&out);
}
