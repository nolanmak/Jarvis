//! #1294 — `augmentagent slack deliver` end to end: the real binary against a
//! local mock Slack (`AUGMENTAGENT_SLACK_API_BASE`), a temp database and the
//! insecure file credential store, with credentials installed through
//! `slack app install` against the same mock. Nothing touches a real
//! Keychain or slack.com; tokens and IDs are synthetic.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

const APP: &str = "xapp-test-000";
const BOT: &str = "xoxb-test-000";
const TEAM: &str = "T00000001";
const CHANNEL: &str = "C00000001";
const THREAD: &str = "1700000000.000100";
const TICKET: &str = "ticket-test-000";

#[derive(Default)]
struct Seen {
    posts: Mutex<Vec<Value>>,
    uploads: Mutex<Vec<Vec<u8>>>,
    completes: Mutex<Vec<Value>>,
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    server: mockito::ServerGuard,
    seen: Arc<Seen>,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home dir ü");
        std::fs::create_dir_all(&root).unwrap();
        let mut server = mockito::Server::new();
        let seen = Arc::new(Seen::default());
        mock_slack(&mut server, &seen);
        let env = Env {
            _tmp: tmp,
            root,
            server,
            seen,
        };
        let out = env.run_stdin(
            &["slack", "app", "install", "--stdin", "--json"],
            &format!("{APP}\n{BOT}\n"),
        );
        assert!(out.status.success(), "install: {}", text(&out));
        env
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        c.current_dir(&self.root)
            .env("HOME", &self.root)
            .env("AUGMENTAGENT_DB", self.root.join("agent.db"))
            .env(
                "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
                self.root.join("cred store"),
            )
            .env("AUGMENTAGENT_SLACK_API_BASE", self.server.url())
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env("RUST_LOG", "debug")
            .env_remove("AUGMENTAGENT_SLACK_APP_TOKEN")
            .env_remove("AUGMENTAGENT_SLACK_BOT_TOKEN")
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        let out = self.cmd(args).stdin(Stdio::null()).output().unwrap();
        assert_clean(&out);
        out
    }

    fn run_stdin(&self, args: &[&str], stdin: &str) -> Output {
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
        assert_clean(&out);
        out
    }

    fn write(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn post_texts(&self) -> Vec<String> {
        self.seen
            .posts
            .lock()
            .unwrap()
            .iter()
            .map(|p| p["text"].as_str().unwrap().to_string())
            .collect()
    }
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
        for secret in [APP, BOT, TICKET, "/upload/v1/"] {
            assert!(!s.contains(secret), "{secret} leaked into output:\n{s}");
        }
    }
}

fn json_out(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}", text(out)))
}

fn mock_slack(server: &mut mockito::ServerGuard, seen: &Arc<Seen>) {
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
        .match_header("authorization", format!("Bearer {APP}").as_str())
        .with_body(
            json!({"ok": true,
                "url": format!("wss://wss.example.test/link/?ticket={TICKET}&app_id=A00000001")})
            .to_string(),
        )
        .create();

    let counter = Arc::new(AtomicUsize::new(100));
    let s = Arc::clone(seen);
    server
        .mock("POST", "/chat.postMessage")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            s.posts.lock().unwrap().push(body.clone());
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            json!({"ok": true, "channel": body["channel"], "ts": format!("1700000001.{n:06}")})
                .to_string()
                .into_bytes()
        })
        .create();
    let upload_url = format!("{}/upload/v1/test", server.url());
    server
        .mock("POST", "/files.getUploadURLExternal")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_body(
            json!({"ok": true, "upload_url": upload_url, "file_id": "F00000001"}).to_string(),
        )
        .create();
    let s = Arc::clone(seen);
    server
        .mock("POST", "/upload/v1/test")
        .match_header("authorization", mockito::Matcher::Missing)
        .with_body_from_request(move |req| {
            s.uploads.lock().unwrap().push(req.body().unwrap().clone());
            b"OK".to_vec()
        })
        .create();
    let s = Arc::clone(seen);
    server
        .mock("POST", "/files.completeUploadExternal")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_body_from_request(move |req| {
            let body: Value = serde_json::from_slice(req.body().unwrap()).unwrap();
            s.completes.lock().unwrap().push(body);
            json!({"ok": true, "files": [{"id": "F00000001", "title": "report"}]})
                .to_string()
                .into_bytes()
        })
        .create();
}

fn long_answer() -> String {
    let mut md =
        String::from("# Weekly findings\n\nHey @channel <!here>, the **full** write-up.\n\n");
    for i in 0..10 {
        md.push_str(&format!(
            "Paragraph {i}: {} & notes.\n\n",
            "lorem ipsum dolor sit amet ".repeat(8)
        ));
    }
    md.push_str("```python\n");
    for i in 0..100 {
        md.push_str(&format!("value_{i} = compute({i}) * 2  # <T> & co\n"));
    }
    md.push_str("```\n\n- done\n");
    md
}

#[test]
fn deliver_splits_neutralises_uploads_and_a_rerun_sends_nothing() {
    let env = Env::new();
    let answer = env.write("answer dir/answer ü.md", long_answer().as_bytes());
    let report = env.write("generated ü/Q3 report 日本.txt", b"synthetic report bytes");
    let args = [
        "slack",
        "deliver",
        "--channel",
        CHANNEL,
        "--thread",
        THREAD,
        "--text-file",
        answer.to_str().unwrap(),
        "--file",
        report.to_str().unwrap(),
        "--part-chars",
        "1500",
        "--json",
    ];
    let out = env.run(&args);
    assert!(out.status.success(), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["ok"], json!(true), "{v}");
    let parts = v["parts"].as_u64().unwrap() as usize;
    assert!(parts >= 5, "long answer split into several parts: {v}");
    assert_eq!(v["sent_now"], json!(parts));
    assert_eq!(v["newly_enqueued"], json!(parts));

    let texts = env.post_texts();
    assert_eq!(texts.len(), parts - 1, "one post per text part");
    for (i, p) in env.seen.posts.lock().unwrap().iter().enumerate() {
        assert_eq!(p["channel"], json!(CHANNEL));
        assert_eq!(p["thread_ts"], json!(THREAD), "part {i} in thread");
        assert_eq!(p["link_names"], json!(false));
        let key = &p["metadata"]["event_payload"]["idempotency_key"];
        assert!(
            key.as_str().unwrap().ends_with(&format!(":text:{i}")),
            "{key}"
        );
    }
    let all: String = texts.concat();
    assert!(all.contains("@\u{2060}channel") && all.contains("&lt;!here&gt;"));
    assert!(!all.contains("<!here>") && !all.contains("**full**"));
    for i in 0..100 {
        let line = format!("value_{i} = compute({i}) * 2  # &lt;T&gt; &amp; co");
        assert_eq!(all.matches(&line).count(), 1, "{line}");
    }
    assert!(
        texts.iter().any(|t| t.starts_with("```\n")),
        "a code block split across parts is reopened"
    );
    for t in &texts {
        assert!(t.chars().count() <= 1500);
        assert_eq!(t.matches("```").count() % 2, 0, "balanced fences: {t}");
    }
    assert_eq!(
        env.seen.uploads.lock().unwrap().as_slice(),
        &[b"synthetic report bytes".to_vec()]
    );
    let completes = env.seen.completes.lock().unwrap().clone();
    assert_eq!(completes.len(), 1);
    assert_eq!(completes[0]["channel_id"], json!(CHANNEL));
    assert_eq!(completes[0]["thread_ts"], json!(THREAD));

    // Same command again: same turn, same keys, nothing re-sent.
    let again = env.run(&args);
    assert!(again.status.success(), "{}", text(&again));
    let v2 = json_out(&again);
    assert_eq!(v2["turn_id"], v["turn_id"]);
    assert_eq!(v2["sent_now"], json!(0));
    assert_eq!(v2["already_enqueued"], json!(parts));
    assert_eq!(env.post_texts().len(), parts - 1);
    assert_eq!(env.seen.uploads.lock().unwrap().len(), 1);

    // The database never holds a token.
    let db = std::fs::read(env.root.join("agent.db")).unwrap();
    let wal = std::fs::read(env.root.join("agent.db-wal")).unwrap_or_default();
    for bytes in [db, wal] {
        assert!(!String::from_utf8_lossy(&bytes).contains(BOT));
    }
}

#[test]
fn human_output_and_stdin_input() {
    let env = Env::new();
    let out = env.run_stdin(
        &["slack", "deliver", "--channel", CHANNEL, "--stdin"],
        "Short **answer** for @here.",
    );
    assert!(out.status.success(), "{}", text(&out));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("1 part(s)"), "{s}");
    assert!(s.contains("(sent now)"), "{s}");
    assert_eq!(env.post_texts(), ["Short *answer* for @\u{2060}here."]);
    assert!(env.seen.posts.lock().unwrap()[0].get("thread_ts").is_none());
}

#[test]
fn failures_exit_nonzero_with_recovery() {
    let env = Env::new();
    // Nothing to send.
    let out = env.run(&["slack", "deliver", "--channel", CHANNEL, "--json"]);
    assert!(!out.status.success());
    let v = json_out(&out);
    assert_eq!(v["error"], json!("text_input"));
    assert!(v["recovery"].as_str().is_some_and(|r| !r.is_empty()));

    // Not a Slack conversation ID / ts.
    let f = env.write("a.md", b"hi");
    let out = env.run(&[
        "slack",
        "deliver",
        "--channel",
        "general channel",
        "--text-file",
        f.to_str().unwrap(),
        "--json",
    ]);
    assert!(!out.status.success());
    assert_eq!(json_out(&out)["error"], json!("invalid_target"));

    // Unknown workspace.
    let out = env.run(&[
        "slack",
        "deliver",
        "--team",
        "T00000009",
        "--channel",
        CHANNEL,
        "--text-file",
        f.to_str().unwrap(),
        "--json",
    ]);
    assert!(!out.status.success());
    let v = json_out(&out);
    assert!(v["recovery"].as_str().is_some_and(|r| !r.is_empty()), "{v}");
    assert!(env.post_texts().is_empty());

    // A missing file is refused at send time: the text goes, the file is
    // dead-lettered and the exit status says so.
    let out = env.run(&[
        "slack",
        "deliver",
        "--channel",
        CHANNEL,
        "--text-file",
        f.to_str().unwrap(),
        "--file",
        env.root.join("missing ü.pdf").to_str().unwrap(),
        "--json",
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["sends"][0]["status"], json!("sent"));
    assert_eq!(v["sends"][1]["status"], json!("dead_letter"));
    assert!(v["sends"][1]["hint"]
        .as_str()
        .unwrap()
        .contains("--turn-id"));
}
