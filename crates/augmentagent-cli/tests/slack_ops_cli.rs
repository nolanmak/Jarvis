//! #1299 — Slack operations through the built binary: `status` and `doctor`
//! walk the owner through each setup step with a recovery command, flag the
//! plaintext test credential store and a Discord token without a channel ID
//! (from this process and from the running daemon's own start report), show
//! reconnects and prove credentials usable only once the daemon connected;
//! and the logs of a representative Slack flow carry no token, socket
//! ticket or message body.
//!
//! Same fixture style as `slack_serve_cli.rs`: a local fake Slack (mockito
//! for the Web API, an in-test WebSocket server for Socket Mode), the
//! plaintext test credential store (`AUGMENTAGENT_INSECURE_CREDENTIAL_DIR`,
//! never the Keychain) and the debug-build turn stub, so no reasoner or
//! provider runs. Tokens, IDs and message texts are synthetic.

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

const APP: &str = "xapp-test-1299";
const BOT: &str = "xoxb-test-1299";
const TICKET: &str = "ticket-test-1299";
const DISCORD_TOKEN: &str = "discord-test-1299-not-a-token";
const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const OWNER_DM: &str = "D00000001";
const STRANGER_DM: &str = "D00000002";
/// Message bodies that must never reach a log.
const OWNER_BODY: &str = "canary owner body 7f3a";
const STRANGER_BODY: &str = "canary stranger body 91c2";
const SCOPES: &str = "app_mentions:read,channels:history,channels:read,chat:write,commands,\
files:read,files:write,groups:history,groups:read,im:history,im:read,im:write,mpim:history,mpim:read,\
reactions:write,users:read";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    api: String,
    /// Everything every command printed, for the log scan.
    transcript: std::sync::Mutex<String>,
}

impl Env {
    fn new(api: String) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state dir & <ü>");
        std::fs::create_dir_all(root.join("home")).unwrap();
        Env {
            _tmp: tmp,
            root,
            api,
            transcript: Default::default(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        c.current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg state"))
            .env("AUGMENTAGENT_DB", self.root.join("agent.db"))
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
            .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
            .env("CLAUDE_CLI", self.root.join("no-such-claude"))
            .env("AUGMENTAGENT_TEST_SLACK_TURN_REPLY", "STUB:")
            .env("DASHBOARD_PORT", "1")
            .env("NO_COLOR", "1")
            // The level the installed services use, with the Slack, store
            // and credential crates turned all the way up: even their trace
            // output must stay free of secrets and bodies.
            .env(
                "RUST_LOG",
                "info,augmentagent_channel_slack=trace,augmentagent_store=trace,augmentagent_auth=trace",
            )
            .args(args);
        c
    }

    fn record(&self, out: &Output) {
        let mut t = self.transcript.lock().unwrap();
        t.push_str(&String::from_utf8_lossy(&out.stdout));
        t.push_str(&String::from_utf8_lossy(&out.stderr));
    }

    fn run(&self, args: &[&str], stdin: &str, extra_env: &[(&str, &str)]) -> Output {
        let mut child = self
            .cmd(args)
            .envs(extra_env.iter().copied())
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
        self.record(&out);
        out
    }

    fn ok(&self, args: &[&str], stdin: &str) {
        let out = self.run(args, stdin, &[]);
        assert!(
            out.status.success(),
            "{args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn json(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Value {
        let mut last = String::new();
        for _ in 0..20 {
            let out = self.run(args, "", extra_env);
            match serde_json::from_slice(&out.stdout) {
                Ok(v) => return v,
                Err(e) => last = format!("{e}: {}", String::from_utf8_lossy(&out.stderr)),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("{args:?} JSON never parsed: {last}");
    }

    fn status(&self) -> Value {
        self.json(&["status", "--json", "true"], &[])
    }

    fn doctor(&self, extra: &[&str], extra_env: &[(&str, &str)]) -> Value {
        let mut args = vec!["doctor", "--json", "true"];
        args.extend_from_slice(extra);
        self.json(&args, extra_env)
    }

    fn serve(&self, env: &[(&str, &str)]) -> Child {
        self.cmd(&["serve", "--no-email", "true", "--dry-run", "false"])
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

fn finding<'a>(doctor: &'a Value, name: &str) -> &'a Value {
    doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!(name))
        .unwrap_or_else(|| panic!("no doctor finding {name}: {doctor:#}"))
}

fn issue<'a>(status: &'a Value, id: &str, source: &str) -> Option<&'a Value> {
    status["config_issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id) && i["source"] == json!(source))
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

struct FakeSocket {
    port: u16,
    send: mpsc::UnboundedSender<Value>,
    acks: mpsc::UnboundedReceiver<String>,
    /// Closing the current connection (the client reconnects).
    drop_link: mpsc::UnboundedSender<()>,
}

async fn fake_socket() -> FakeSocket {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Value>();
    let (ack_tx, ack_rx) = mpsc::unbounded_channel::<String>();
    let (drop_tx, mut drop_rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                continue;
            };
            let hello = json!({"type": "hello", "num_connections": 1,
                "connection_info": {"app_id": "A00000001"}});
            ws.send(Message::Text(hello.to_string())).await.unwrap();
            loop {
                tokio::select! {
                    _ = drop_rx.recv() => {
                        let _ = ws.close(None).await;
                        break;
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
        drop_link: drop_tx,
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
                "url": format!("ws://127.0.0.1:{ws_port}/link/?ticket={TICKET}&app_id=A00000001")})
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_and_doctor_name_each_missing_setup_step_with_its_recovery() {
    let socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let env = Env::new(server.url());

    // 1. Nothing installed.
    let s = env.status();
    let slack = &s["interactive"]["slack"];
    assert_eq!(slack["state"], json!("not_configured"));
    assert_eq!(
        slack["detail"],
        json!("no interactive Slack app is installed")
    );
    assert_eq!(slack["app_installed"], json!(false));
    assert_eq!(slack["credentials"], json!("missing"));
    assert!(slack["recovery"]
        .as_str()
        .unwrap()
        .contains("slack app install --stdin"));
    // The plaintext test store is loud, in status and in doctor.
    assert_eq!(s["credentials"]["backend"], json!("insecure-file"));
    assert_eq!(s["credentials"]["insecure_file_store"], json!(true));
    let insecure = issue(&s, "credentials.insecure_file_store", "cli").expect("insecure issue");
    assert_eq!(insecure["severity"], json!("error"));
    assert_eq!(s["daemon_report"], Value::Null);
    let d = env.doctor(&[], &[]);
    assert_eq!(
        finding(&d, "credential_backend")["severity"],
        json!("error")
    );
    assert_eq!(
        finding(&d, "config.credentials.insecure_file_store")["severity"],
        json!("error")
    );
    assert_eq!(
        finding(&d, "interactive.slack.credentials")["severity"],
        json!("ok")
    );

    // 2. Installed, no owner: the detail names the bind step, not install.
    env.ok(
        &["slack", "app", "install", "--stdin"],
        &format!("{APP}\n{BOT}\n"),
    );
    let slack = env.status()["interactive"]["slack"].clone();
    assert_eq!(slack["state"], json!("not_configured"));
    assert_eq!(
        slack["detail"],
        json!("the Slack app is installed but no owner is bound")
    );
    assert_eq!(slack["app_installed"], json!(true));
    assert_eq!(slack["owner_bound"], json!(false));
    assert_eq!(slack["credentials"], json!("present"));
    let recovery = slack["recovery"].as_str().unwrap();
    assert!(
        recovery.contains("slack app owner bind --user"),
        "{recovery}"
    );
    assert!(!recovery.contains("slack app install"), "{recovery}");

    // 3. Owner bound, daemon not running: disconnected, credentials not
    // claimed usable, doctor points at the daemon-context check; --deep
    // compares the stored grant with the required scopes.
    env.ok(&["slack", "app", "owner", "bind", "--user", OWNER], "");
    let slack = env.status()["interactive"]["slack"].clone();
    assert_eq!(slack["state"], json!("disconnected"));
    assert_eq!(slack["owner_bound"], json!(true));
    assert_eq!(slack["credentials"], json!("present"));
    assert_eq!(slack["reconnects"], Value::Null);
    let d = env.doctor(&["--deep"], &[]);
    let creds = finding(&d, "interactive.slack.credentials");
    assert_eq!(creds["severity"], json!("warn"));
    assert!(creds["suggested_cmd"].is_string());
    let scopes = finding(&d, "slack_app.scopes");
    assert_eq!(scopes["severity"], json!("ok"), "{scopes}");

    // 4. A Discord token without DISCORD_CHANNEL_ID.
    let discord = [("DISCORD_BOT_TOKEN", DISCORD_TOKEN)];
    let s = env.json(&["status", "--json", "true"], &discord);
    let i = issue(&s, "discord.approval_broker", "cli").expect("discord issue");
    assert!(i["detail"].as_str().unwrap().contains("DISCORD_CHANNEL_ID"));
    assert!(i["recovery"]
        .as_str()
        .unwrap()
        .contains("service --unit daemon restart"));
    let d = env.doctor(&[], &discord);
    assert_eq!(
        finding(&d, "config.discord.approval_broker")["severity"],
        json!("warn")
    );
    let with_channel = [
        ("DISCORD_BOT_TOKEN", DISCORD_TOKEN),
        ("DISCORD_CHANNEL_ID", "4242"),
    ];
    let s = env.json(&["status", "--json", "true"], &with_channel);
    assert!(issue(&s, "discord.approval_broker", "cli").is_none());

    assert_clean(&env.transcript.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_daemon_reports_its_store_and_workarounds_and_logs_stay_clean() {
    let mut socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let answer = server
        .mock("POST", "/chat.postMessage")
        .match_body(Matcher::PartialJson(json!({"channel": OWNER_DM})))
        .with_body(json!({"ok": true, "channel": OWNER_DM, "ts": "1800000000.000001"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let rejection = server
        .mock("POST", "/chat.postMessage")
        .match_body(Matcher::PartialJson(json!({"channel": STRANGER_DM})))
        .with_body(
            json!({"ok": true, "channel": STRANGER_DM, "ts": "1800000000.000002"}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let env = Env::new(server.url());
    env.ok(
        &["slack", "app", "install", "--stdin"],
        &format!("{APP}\n{BOT}\n"),
    );
    env.ok(&["slack", "app", "owner", "bind", "--user", OWNER], "");

    // Discord token without a channel ID: serve logs it and runs on.
    let mut serve = env.serve(&[("DISCORD_BOT_TOKEN", DISCORD_TOKEN)]);
    let pid = serve.id();
    eventually("connected", Duration::from_secs(30), || {
        env.status()["interactive"]["slack"]["state"] == json!("connected")
    })
    .await;
    let s = env.status();
    let slack = &s["interactive"]["slack"];
    assert_eq!(slack["credentials"], json!("usable"), "{slack}");
    assert_eq!(slack["reconnects"], json!(0));
    let report = &s["daemon_report"];
    assert_eq!(report["pid"], json!(pid));
    assert_eq!(report["running"], json!(true));
    assert_eq!(report["credential_backend"], json!("insecure-file"));
    assert_eq!(report["insecure_file_store"], json!(true));
    let daemon_insecure =
        issue(&s, "daemon.insecure_file_store", "daemon").expect("daemon insecure issue");
    assert_eq!(daemon_insecure["severity"], json!("error"));
    let broker = issue(&s, "discord.approval_broker", "daemon").expect("daemon discord notice");
    assert!(broker["detail"]
        .as_str()
        .unwrap()
        .contains("DISCORD_CHANNEL_ID"));
    assert!(broker["recovery"].is_string());
    let d = env.doctor(&[], &[]);
    assert_eq!(
        finding(&d, "config.daemon.insecure_file_store")["severity"],
        json!("error")
    );
    assert_eq!(
        finding(&d, "interactive.slack.credentials")["severity"],
        json!("ok")
    );

    // A representative flow: an owner turn answered, a stranger rejected.
    socket
        .send
        .send(dm(
            "env-1",
            OWNER_DM,
            OWNER,
            OWNER_BODY,
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
            STRANGER_BODY,
            "1800000001.000200",
        ))
        .unwrap();
    assert_eq!(socket.ack().await, "env-2");
    eventually(
        "answer and rejection posted",
        Duration::from_secs(15),
        || answer.matched() && rejection.matched(),
    )
    .await;

    // The link drops once; the daemon reconnects and counts it.
    socket.drop_link.send(()).unwrap();
    eventually("reconnected once", Duration::from_secs(30), || {
        let slack = env.status()["interactive"]["slack"].clone();
        slack["state"] == json!("connected") && slack["reconnects"] == json!(1)
    })
    .await;

    let _ = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(20);
    while serve.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = serve.kill();
    let out = serve.wait_with_output().unwrap();
    env.record(&out);
    let serve_log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Once the daemon is gone its report is history, not a current issue.
    let s = env.status();
    assert_eq!(s["daemon_report"]["running"], json!(false));
    assert!(issue(&s, "daemon.insecure_file_store", "daemon").is_none());

    // Turn correlation: every owner-turn line names its durable event seq.
    let turn_lines: Vec<&str> = serve_log
        .lines()
        .filter(|l| l.contains("slack interactive: owner turn"))
        .collect();
    assert!(
        !turn_lines.is_empty(),
        "no owner-turn log line:\n{serve_log}"
    );
    assert!(
        turn_lines.iter().all(|l| l.contains("seq=")),
        "{turn_lines:?}"
    );
    // Known gap, pinned so it is explicit: the owner-turn line carries the
    // durable seq (which resolves to workspace and conversation in the
    // database) but not the team or channel ID itself. The turn path is
    // being replaced in #1288; drop this assertion when it logs them.
    assert!(
        turn_lines.iter().all(|l| !l.contains("team=")),
        "the owner-turn line now logs the workspace; update this test: {turn_lines:?}"
    );

    answer.assert_async().await;
    rejection.assert_async().await;
    assert_clean(&env.transcript.lock().unwrap());
}

/// No token, socket ticket or message body anywhere in what the commands
/// printed, at the service log level with Slack/store/auth at trace.
fn assert_clean(transcript: &str) {
    assert!(!transcript.is_empty());
    for secret in [APP, BOT, TICKET, DISCORD_TOKEN] {
        assert!(
            !transcript.contains(secret),
            "secret {secret} appears in command output"
        );
    }
    for body in [OWNER_BODY, STRANGER_BODY] {
        assert!(
            !transcript.contains(body),
            "message body {body:?} appears in command output"
        );
    }
}

/// #1299 — a credential store this process cannot read (the macOS case is a
/// session without access to the login Keychain) is reported as unknown,
/// never as an installed app. Reproduced here without the Keychain by making
/// the plaintext test store's slot directory unreadable.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_credential_store_is_unknown_not_installed() {
    use std::os::unix::fs::PermissionsExt;
    let socket = fake_socket().await;
    let mut server = mockito::Server::new_async().await;
    mock_slack(&mut server, socket.port).await;
    let env = Env::new(server.url());
    env.ok(
        &["slack", "app", "install", "--stdin"],
        &format!("{APP}\n{BOT}\n"),
    );
    let slot_dir = env.root.join("creds").join("slack-app");
    std::fs::set_permissions(&slot_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&slot_dir).is_ok() {
        // Running as root: permissions are not enforced, nothing to check.
        std::fs::set_permissions(&slot_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let s = env.status();
    let d = env.doctor(&[], &[]);
    std::fs::set_permissions(&slot_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    let slack = &s["interactive"]["slack"];
    assert_eq!(slack["app_installed"], Value::Null, "{slack}");
    assert_eq!(slack["credentials"], json!("unreadable"));
    assert!(slack["detail"]
        .as_str()
        .unwrap()
        .contains("cannot read the credential store"));
    assert!(slack["recovery"]
        .as_str()
        .unwrap()
        .contains("augmentagent doctor --keychain-probe"));
    let unreadable = issue(&s, "credentials.unreadable", "cli").expect("unreadable issue");
    assert!(unreadable["detail"]
        .as_str()
        .unwrap()
        .contains("augmentagent/slack-app/_installs"));
    assert_eq!(
        finding(&d, "interactive.slack.credentials")["severity"],
        json!("warn")
    );
    assert_eq!(
        finding(&d, "config.credentials.unreadable")["severity"],
        json!("warn")
    );

    // Readable again: installed, as before.
    let slack = env.status()["interactive"]["slack"].clone();
    assert_eq!(slack["app_installed"], json!(true));
    assert_clean(&env.transcript.lock().unwrap());
}
