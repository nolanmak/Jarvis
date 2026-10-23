//! #1293 — `augmentagent slack files fetch` end to end: the real binary
//! against a local mock Slack (Web API + file host on loopback, allowed with
//! the test-only `AUGMENTAGENT_SLACK_TEST_FILE_HOSTS`), a temp database,
//! state dir and insecure credential store, with the app installed through
//! `slack app install` against the same mock. PDF conversion uses a fake
//! `pdftotext` first on `PATH`. Tokens and IDs are synthetic.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mockito::Matcher;
use serde_json::{json, Value};

const APP: &str = "xapp-test-000";
const BOT: &str = "xoxb-test-000";
const TEAM: &str = "T00000001";
const DM: &str = "D00000001";
const TS: &str = "1700000000.000200";

const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

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
        let fake = tools.join("pdftotext");
        std::fs::write(&fake, "#!/bin/sh\necho 'SYNTHETIC PDF TEXT'\n").unwrap();
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
        );
        assert!(out.status.success(), "install: {}", text(&out));
        env
    }

    fn host(&self) -> String {
        self.server.url().trim_start_matches("http://").to_string()
    }

    fn inbound_root(&self) -> PathBuf {
        self.root.join("state/augmentagent/slack-inbound")
    }

    fn cmd(&self, args: &[&str]) -> Command {
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
            .env_remove("MISTRAL_API_KEY")
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

    /// Serve one message (`conversations.replies` for `ts`) with `files`.
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

    fn serve(&mut self, path: &str, content_type: &str, body: &[u8]) -> mockito::Mock {
        self.server
            .mock("GET", format!("/files-pri/{TEAM}-{path}").as_str())
            .match_header("authorization", format!("Bearer {BOT}").as_str())
            .with_header("content-type", content_type)
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

fn file_json(id: &str, name: &str, mimetype: &str, size: usize, url: &str) -> Value {
    json!({"id": id, "name": name, "mimetype": mimetype, "size": size, "mode": "hosted",
        "is_external": false, "url_private": url, "url_private_download": url})
}

fn is_empty_dir(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

#[test]
fn fetch_builds_the_turn_for_an_attachment_only_message_and_cleans_up() {
    let mut env = Env::new();
    let png = env.serve("F1/shot.png", "image/png", PNG_1X1);
    let txt = env.serve("F2/notes.txt", "text/plain", b"synthetic notes");
    let pdf = env.serve("F3/report.pdf", "application/pdf", b"%PDF-1.4 synthetic");
    let files = json!([
        file_json(
            "F1",
            "shot.png",
            "image/png",
            PNG_1X1.len(),
            &env.file_url("F1/shot.png")
        ),
        file_json(
            "F2",
            "notes.txt",
            "text/plain",
            15,
            &env.file_url("F2/notes.txt")
        ),
        file_json(
            "F3",
            "Report Q3.pdf",
            "application/pdf",
            18,
            &env.file_url("F3/report.pdf")
        ),
        file_json(
            "F4",
            "bundle.zip",
            "application/zip",
            10,
            &env.file_url("F4/bundle.zip")
        ),
        file_json(
            "F5",
            "huge.log",
            "text/plain",
            9 * 1024 * 1024,
            &env.file_url("F5/huge.log")
        ),
    ]);
    env.message(TS, "", files);
    let out = env.run(&[
        "slack",
        "files",
        "fetch",
        "--channel",
        DM,
        "--ts",
        TS,
        "--json",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["text"], json!(""));
    assert_eq!(v["starts_turn"], json!(true));
    let kinds: Vec<&str> = v["accepted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["image", "text", "pdf"]);
    assert_eq!(v["accepted"][2]["preview"], json!("SYNTHETIC PDF TEXT"));
    assert_eq!(v["accepted"][2]["name"], json!("Report Q3.pdf"));
    let notice = v["notice"].as_str().unwrap();
    assert!(
        notice.contains("bundle.zip (unsupported: application/zip)"),
        "{notice}"
    );
    assert!(notice.contains("huge.log (9.0 MB > 8.0 MB)"), "{notice}");
    assert!(v["prompt"].as_str().unwrap().contains("IMAGE: "));
    assert_eq!(v["cleaned_up"], json!(true));
    png.assert();
    txt.assert();
    pdf.assert();
    assert!(is_empty_dir(&env.inbound_root()), "files left behind");
}

#[test]
fn a_redirect_to_a_foreign_host_is_refused_and_the_token_never_sent() {
    let mut env = Env::new();
    let mut foreign = mockito::Server::new();
    let never = foreign.mock("GET", Matcher::Any).expect(0).create();
    env.server
        .mock("GET", format!("/files-pri/{TEAM}-F1/a.txt").as_str())
        .with_status(302)
        .with_header("location", &format!("{}/collect", foreign.url()))
        .create();
    let files = json!([file_json(
        "F1",
        "a.txt",
        "text/plain",
        3,
        &env.file_url("F1/a.txt")
    )]);
    env.message(TS, "read this", files);
    let out = env.run(&[
        "slack",
        "files",
        "fetch",
        "--channel",
        DM,
        "--ts",
        TS,
        "--json",
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let v = json_out(&out);
    assert_eq!(v["accepted"], json!([]));
    assert!(
        v["notice"]
            .as_str()
            .unwrap()
            .contains("not a Slack file host"),
        "{v}"
    );
    never.assert();
    assert!(is_empty_dir(&env.inbound_root()));
}

#[test]
fn unknown_message_and_non_loopback_test_host_fail_clearly() {
    let env = Env::new();
    let out = env.run(&[
        "slack",
        "files",
        "fetch",
        "--channel",
        DM,
        "--ts",
        "1700000000.999999",
        "--json",
    ]);
    assert!(!out.status.success());
    let v = json_out(&out);
    assert_eq!(v["error"], json!("slack_api"), "{v}");
    let out = env
        .cmd(&[
            "slack",
            "files",
            "fetch",
            "--channel",
            DM,
            "--ts",
            TS,
            "--json",
        ])
        .env(
            "AUGMENTAGENT_SLACK_TEST_FILE_HOSTS",
            "files.example.com:443",
        )
        .output()
        .unwrap();
    assert!(!out.status.success());
    let v = json_out(&out);
    assert_eq!(v["error"], json!("invalid_test_file_host"), "{v}");
}
