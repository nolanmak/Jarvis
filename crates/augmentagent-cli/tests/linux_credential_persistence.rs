//! #1325 — on Linux, Slack app tokens stored by one `augmentagent` process
//! are read by the next one, through the default credential store of the
//! real binary (built with the workspace's unified keyring features, which
//! before #1325 meant the D-Bus Secret Service with no session bus here).
//!
//! The binary runs against a local mock Slack with a temporary HOME and
//! XDG_STATE_HOME, no D-Bus session and no plaintext override, so nothing
//! touches the runner's state, a keyring or slack.com. Tokens are synthetic.
//!
//! Linux only (runs in the `ubuntu-latest` platform job); it compiles on
//! macOS but is ignored there, where the default store is the login Keychain.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

const APP: &str = "xapp-test-1325";
const BOT: &str = "xoxb-test-1325";
const TEAM: &str = "T00000001";
const SCOPES: &str = "app_mentions:read,channels:history,channels:read,chat:write,commands,\
files:read,files:write,groups:history,groups:read,im:history,im:read,im:write,mpim:history,mpim:read,\
reactions:write,users:read";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    server: mockito::ServerGuard,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home dir ü");
        std::fs::create_dir_all(root.join("home")).unwrap();
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/auth.test")
            .match_header("authorization", format!("Bearer {BOT}").as_str())
            .with_header("x-oauth-scopes", SCOPES)
            .with_body(
                json!({
                    "ok": true, "url": "https://example-test.slack.com/",
                    "team": "Example Test", "user": "jarvis", "team_id": TEAM,
                    "user_id": "U00000001", "bot_id": "B00000001"
                })
                .to_string(),
            )
            .create();
        server
            .mock("POST", "/apps.connections.open")
            .match_header("authorization", format!("Bearer {APP}").as_str())
            .with_body(
                json!({
                    "ok": true,
                    "url": "wss://wss.example.test/link/?ticket=ticket-test-1325&app_id=A00000001"
                })
                .to_string(),
            )
            .create();
        Env {
            _tmp: tmp,
            root,
            server,
        }
    }

    fn state_home(&self) -> PathBuf {
        self.root.join("xdg state")
    }

    /// A fresh process each call: nothing is shared but the filesystem.
    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.state_home())
            .env("AUGMENTAGENT_DB", self.root.join("agent.db"))
            .env("AUGMENTAGENT_SLACK_API_BASE", self.server.url())
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env("DASHBOARD_PORT", "1")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "debug,augmentagent_auth=trace")
            .args(args)
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
        for stream in [&out.stdout, &out.stderr] {
            let text = String::from_utf8_lossy(stream);
            for secret in [APP, BOT] {
                assert!(!text.contains(secret), "{secret} leaked:\n{text}");
            }
        }
        out
    }

    fn json(&self, args: &[&str], stdin: &str) -> Value {
        let out = self.run(args, stdin);
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: stdout is not JSON ({e}):\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        })
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "Linux only: on macOS the default store is the login Keychain"
)]
fn slack_tokens_stored_by_install_are_read_by_a_later_process() {
    let env = Env::new();

    let v = env.json(
        &["slack", "app", "install", "--stdin", "--json"],
        &format!("{APP}\n{BOT}\n"),
    );
    assert_eq!(v["ok"], json!(true), "{v:#}");
    assert_eq!(v["credential_backend"], json!("private-file"), "{v:#}");

    // Owner-only files under the state directory, nothing under HOME.
    let dir = env.state_home().join("augmentagent/credentials");
    assert_eq!(mode(&dir), 0o700);
    for entry in std::fs::read_dir(&dir).unwrap() {
        let platform = entry.unwrap().path();
        assert_eq!(mode(&platform), 0o700, "{}", platform.display());
        for file in std::fs::read_dir(&platform).unwrap() {
            let file = file.unwrap().path();
            assert_eq!(mode(&file), 0o600, "{}", file.display());
        }
    }
    assert!(!env.root.join("home/.local/state").exists());

    // A second process sees the install and can read its tokens.
    let v = env.json(&["slack", "app", "status", "--json"], "");
    let installs = v["installs"].as_array().expect("installs");
    assert_eq!(installs.len(), 1, "{v:#}");
    assert_eq!(installs[0]["team_id"], json!(TEAM));
    assert_eq!(installs[0]["credentials"], json!("ok"), "{v:#}");

    // status and doctor name the backend and call it persistent.
    let s = env.json(&["status", "--json", "true"], "");
    assert_eq!(s["credentials"]["backend"], json!("private-file"), "{s:#}");
    assert_eq!(s["credentials"]["persistent"], json!(true));
    assert_eq!(s["credentials"]["insecure_file_store"], json!(false));
    let d = env.json(&["doctor", "--json", "true"], "");
    let finding = d["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("credential_backend"))
        .unwrap_or_else(|| panic!("no credential_backend finding: {d:#}"));
    assert_eq!(finding["severity"], json!("ok"), "{finding:#}");
    assert!(finding["message"]
        .as_str()
        .unwrap()
        .contains("private-file (persistent)"));
}
