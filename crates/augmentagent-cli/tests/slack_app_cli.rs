//! #1284 — `augmentagent slack app …` end to end: the real binary against a
//! local mock Slack (`AUGMENTAGENT_SLACK_API_BASE`), a temp database and the
//! insecure file credential store (`AUGMENTAGENT_INSECURE_CREDENTIAL_DIR`).
//! Nothing touches a real Keychain, keyring or slack.com. Tokens are
//! synthetic.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use augmentagent_auth::{CredentialStore, FileCredentialStore};
use augmentagent_store::Store;
use serde_json::{json, Value};

const APP: &str = "xapp-test-000";
const APP2: &str = "xapp-test-001";
const BOT: &str = "xoxb-test-000";
const BOT2: &str = "xoxb-test-001";
const BOT_OTHER_TEAM: &str = "xoxb-test-999";
const BOT_FEW: &str = "xoxb-test-few";
const BOT_REVOKED: &str = "xoxb-test-bad";
const TEAM: &str = "T00000001";
const TICKET: &str = "ticket-test-000";
const ALL_TOKENS: &[&str] = &[APP, APP2, BOT, BOT2, BOT_OTHER_TEAM, BOT_FEW, BOT_REVOKED];

const ALL_SCOPES: &str = "app_mentions:read,channels:history,channels:read,chat:write,commands,\
files:read,files:write,groups:history,groups:read,im:history,im:read,mpim:history,mpim:read,\
reactions:write,users:read";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    creds: PathBuf,
    server: mockito::ServerGuard,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Spaces and non-ASCII in every path the CLI touches.
        let root = tmp.path().join("home dir ü");
        std::fs::create_dir_all(&root).unwrap();
        let creds = root.join("cred store");
        let mut server = mockito::Server::new();
        mock_slack(&mut server);
        Env {
            _tmp: tmp,
            root,
            creds,
            server,
        }
    }

    fn db(&self) -> PathBuf {
        self.root.join("agent.db")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        c.current_dir(&self.root)
            .env("HOME", &self.root)
            .env("AUGMENTAGENT_DB", self.db())
            .env("AUGMENTAGENT_INSECURE_CREDENTIAL_DIR", &self.creds)
            .env("AUGMENTAGENT_SLACK_API_BASE", self.server.url())
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env("RUST_LOG", "debug")
            .env_remove("AUGMENTAGENT_SLACK_APP_TOKEN")
            .env_remove("AUGMENTAGENT_SLACK_BOT_TOKEN")
            .arg("slack")
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

    fn file_store(&self) -> FileCredentialStore {
        FileCredentialStore::new(&self.creds)
    }

    /// Every byte the CLI persisted outside the credential store.
    fn assert_db_has_no_tokens(&self) {
        let mut bytes = std::fs::read(self.db()).unwrap_or_default();
        for side in ["agent.db-wal", "agent.db-shm"] {
            bytes.extend(std::fs::read(self.root.join(side)).unwrap_or_default());
        }
        let text = String::from_utf8_lossy(&bytes);
        for t in ALL_TOKENS {
            assert!(!text.contains(t), "token {t} written to the database");
        }
    }
}

fn tokens(app: &str, bot: &str) -> String {
    format!("{app}\n{bot}\n")
}

/// stdout, stderr and debug logs never carry token or ticket bytes.
fn assert_clean(out: &Output) {
    for stream in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(stream);
        for t in ALL_TOKENS.iter().chain([&TICKET]) {
            assert!(!text.contains(t), "secret {t} leaked into output:\n{text}");
        }
    }
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn ok_json(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    stdout_json(out)
}

fn err_json(out: &Output, code: &str) -> Value {
    assert!(!out.status.success(), "expected failure");
    let v = stdout_json(out);
    assert_eq!(v["ok"], json!(false), "{v}");
    assert_eq!(v["error"], json!(code), "{v}");
    assert!(
        v["recovery"].as_str().is_some_and(|r| !r.is_empty()),
        "every failure carries recovery text: {v}"
    );
    v
}

fn who(team: &str) -> Value {
    json!({
        "ok": true,
        "url": "https://example-test.slack.com/",
        "team": if team == TEAM { "Example Test" } else { "Other Test" },
        "user": "jarvis",
        "team_id": team,
        "user_id": "U00000001",
        "bot_id": "B00000001"
    })
}

fn mock_slack(server: &mut mockito::ServerGuard) {
    for (bot, team, scopes) in [
        (BOT, TEAM, ALL_SCOPES),
        (BOT2, TEAM, ALL_SCOPES),
        (BOT_OTHER_TEAM, "T00000002", ALL_SCOPES),
        (BOT_FEW, TEAM, "chat:write,users:read"),
    ] {
        server
            .mock("POST", "/auth.test")
            .match_header("authorization", format!("Bearer {bot}").as_str())
            .with_header("x-oauth-scopes", scopes)
            .with_body(who(team).to_string())
            .create();
    }
    server
        .mock("POST", "/auth.test")
        .match_header("authorization", format!("Bearer {BOT_REVOKED}").as_str())
        .with_body(json!({"ok": false, "error": "invalid_auth"}).to_string())
        .create();
    for app in [APP, APP2] {
        server
            .mock("POST", "/apps.connections.open")
            .match_header("authorization", format!("Bearer {app}").as_str())
            .with_body(
                json!({
                    "ok": true,
                    "url": format!("wss://wss.example.test/link/?ticket={TICKET}&app_id=A00000001")
                })
                .to_string(),
            )
            .create();
    }
}

#[test]
fn install_status_verify_rotate_remove_round_trip() {
    let env = Env::new();

    let v = ok_json(&env.run_stdin(&["app", "install", "--stdin", "--json"], &tokens(APP, BOT)));
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["action"], json!("installed"));
    assert_eq!(v["install"]["team_id"], json!(TEAM));
    assert_eq!(v["install"]["team_name"], json!("Example Test"));
    assert_eq!(v["install"]["bot_user_id"], json!("U00000001"));
    assert_eq!(v["install"]["app_id"], json!("A00000001"));
    assert_eq!(v["install"]["missing_scopes"], json!([]));
    assert_eq!(v["credential_backend"], json!("insecure-file"));
    assert_eq!(v["daemon_credential_access"], json!("unverified"));
    assert!(env.file_store().exists("slack-app", TEAM));

    // Order on stdin does not matter; reinstall over existing state.
    let v = ok_json(&env.run_stdin(&["app", "install", "--stdin", "--json"], &tokens(BOT, APP)));
    assert_eq!(v["action"], json!("reinstalled"));

    let human = env.run_stdin(&["app", "install", "--stdin"], &tokens(APP, BOT));
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("Example Test (T00000001)"), "{text}");
    assert!(text.contains("U00000001"), "{text}");
    assert!(text.contains("all 15 required scopes granted"), "{text}");

    let v = ok_json(&env.run(&["app", "status", "--json"]));
    let installs = v["installs"].as_array().unwrap();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0]["team_id"], json!(TEAM));
    assert_eq!(installs[0]["credentials"], json!("ok"));
    assert_eq!(installs[0]["composio_connected"], json!(false));
    let text = String::from_utf8_lossy(&env.run(&["app", "status"]).stdout).to_string();
    assert!(text.contains(TEAM), "{text}");

    let v = ok_json(&env.run(&["app", "verify", "--json"]));
    assert_eq!(v["install"]["team_id"], json!(TEAM));
    assert!(v["install"]["verified_at"].as_u64().is_some());

    let v = ok_json(&env.run_stdin(
        &["app", "rotate", "--team", TEAM, "--stdin", "--json"],
        &format!("{BOT2}\n"),
    ));
    assert!(v["install"]["rotated_at"].as_u64().is_some());
    let stored = String::from_utf8(env.file_store().get("slack-app", TEAM).unwrap()).unwrap();
    assert!(stored.contains(BOT2) && stored.contains(APP));

    env.assert_db_has_no_tokens();

    let v = ok_json(&env.run(&["app", "remove", "--team", TEAM, "--json"]));
    assert_eq!(v["removed"], json!(true));
    assert!(!env.file_store().exists("slack-app", TEAM));
    let v = ok_json(&env.run(&["app", "disconnect", "--team", TEAM, "--json"]));
    assert_eq!(v["removed"], json!(false), "remove is idempotent");
    let v = ok_json(&env.run(&["app", "status", "--json"]));
    assert_eq!(v["installs"], json!([]));

    let v = err_json(&env.run(&["app", "verify", "--json"]), "nothing_installed");
    assert!(v["recovery"]
        .as_str()
        .unwrap()
        .contains("slack app install"));
}

#[test]
fn wrong_token_type_is_rejected_before_any_network_call() {
    let env = Env::new();
    // Bot token handed where the app-level token belongs.
    let swapped = env.root.join("app token.txt");
    std::fs::write(&swapped, format!("{BOT}\n")).unwrap();
    let bot_file = env.root.join("bot token.txt");
    std::fs::write(&bot_file, format!("{BOT}\n")).unwrap();
    let out = env.run(&[
        "app",
        "install",
        "--app-token-file",
        swapped.to_str().unwrap(),
        "--bot-token-file",
        bot_file.to_str().unwrap(),
        "--json",
    ]);
    let v = err_json(&out, "wrong_token_type");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("app-level token") && msg.contains("bot token"),
        "{msg}"
    );
    assert!(v["recovery"].as_str().unwrap().contains("App-Level Tokens"));

    // Human mode: message and recovery on stderr, non-zero exit.
    let out = env.run_stdin(&["app", "install", "--stdin"], "xoxp-test-000\n");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("user token"), "{err}");
    assert!(err.contains("recovery:"), "{err}");
    assert!(!err.contains("xoxp-test-000"), "{err}");
    assert!(!env.creds.join("slack-app").exists());
}

#[test]
fn token_files_and_env_vars_are_accepted() {
    let env = Env::new();
    let app_file = env.root.join("app token.txt");
    let bot_file = env.root.join("bot token.txt");
    std::fs::write(&app_file, format!("{APP}\n")).unwrap();
    std::fs::write(&bot_file, format!("  {BOT}  \n")).unwrap();
    ok_json(&env.run(&[
        "app",
        "install",
        "--app-token-file",
        app_file.to_str().unwrap(),
        "--bot-token-file",
        bot_file.to_str().unwrap(),
        "--json",
    ]));

    let out = env
        .cmd(&["app", "rotate", "--json"])
        .env("AUGMENTAGENT_SLACK_APP_TOKEN", APP2)
        .env("AUGMENTAGENT_SLACK_BOT_TOKEN", BOT2)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_clean(&out);
    ok_json(&out);
    let stored = String::from_utf8(env.file_store().get("slack-app", TEAM).unwrap()).unwrap();
    assert!(stored.contains(APP2) && stored.contains(BOT2));

    // No token source at all.
    let v = err_json(&env.run(&["app", "install", "--json"]), "token_input");
    assert!(v["recovery"].as_str().unwrap().contains("--stdin"));
}

#[test]
fn missing_scopes_are_named_and_nothing_is_stored() {
    let env = Env::new();
    let out = env.run_stdin(
        &["app", "install", "--stdin", "--json"],
        &tokens(APP, BOT_FEW),
    );
    let v = err_json(&out, "missing_scopes");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("im:history") && msg.contains("app_mentions:read"),
        "{msg}"
    );
    assert!(!msg.contains("chat:write"), "{msg}");
    assert!(v["recovery"].as_str().unwrap().contains("reinstall"));
    assert_eq!(
        ok_json(&env.run(&["app", "status", "--json"]))["installs"],
        json!([])
    );
}

#[test]
fn revoked_token_fails_with_recovery() {
    let env = Env::new();
    let v = err_json(
        &env.run_stdin(
            &["app", "install", "--stdin", "--json"],
            &tokens(APP, BOT_REVOKED),
        ),
        "invalid_token",
    );
    assert!(v["message"].as_str().unwrap().contains("invalid_auth"));
    assert!(v["recovery"]
        .as_str()
        .unwrap()
        .contains("OAuth & Permissions"));
}

#[test]
fn rotate_to_another_workspace_is_refused_and_old_tokens_keep_working() {
    let env = Env::new();
    ok_json(&env.run_stdin(&["app", "install", "--stdin", "--json"], &tokens(APP, BOT)));
    let v = err_json(
        &env.run_stdin(
            &["app", "rotate", "--team", TEAM, "--stdin", "--json"],
            &format!("{BOT_OTHER_TEAM}\n"),
        ),
        "wrong_workspace",
    );
    assert!(v["message"].as_str().unwrap().contains("T00000002"));
    let stored = String::from_utf8(env.file_store().get("slack-app", TEAM).unwrap()).unwrap();
    assert!(stored.contains(BOT));
    ok_json(&env.run(&["app", "verify", "--team", TEAM, "--json"]));
}

fn seed_composio(env: &Env) {
    let store = Store::open(env.db()).unwrap();
    store
        .upsert_slack_workspace(
            TEAM,
            "Example Test",
            "entity-test",
            "conn-test",
            "U00000002",
        )
        .unwrap();
    let payload = json!({
        "entity_id": "entity-test",
        "connection_id": "conn-test",
        "team_id": TEAM,
        "team_name": "Example Test",
        "user_id": "U00000002",
        "composio_api_key": "ck-test-0"
    });
    env.file_store()
        .put("slack", TEAM, payload.to_string().as_bytes())
        .unwrap();
}

fn composio_teams(env: &Env) -> Vec<String> {
    let out = env.run(&["workspaces", "--json", "true"]);
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|w| w["team_id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn removing_the_app_leaves_composio_and_removing_composio_leaves_the_app() {
    let env = Env::new();
    seed_composio(&env);
    let v = ok_json(&env.run_stdin(&["app", "install", "--stdin", "--json"], &tokens(APP, BOT)));
    assert_eq!(v["composio_connected"], json!(true));

    // App removal: Composio row and slot untouched.
    ok_json(&env.run(&["app", "remove", "--team", TEAM, "--json"]));
    assert_eq!(composio_teams(&env), vec![TEAM.to_string()]);
    assert!(env.file_store().exists("slack", TEAM));

    // Composio removal: app slot, index and live verify untouched.
    ok_json(&env.run_stdin(&["app", "install", "--stdin", "--json"], &tokens(APP, BOT)));
    let out = env.run(&["remove-workspace", TEAM]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(composio_teams(&env).is_empty());
    assert!(!env.file_store().exists("slack", TEAM));
    assert!(env.file_store().exists("slack-app", TEAM));
    let v = ok_json(&env.run(&["app", "status", "--json"]));
    assert_eq!(v["installs"][0]["team_id"], json!(TEAM));
    assert_eq!(v["installs"][0]["composio_connected"], json!(false));
    ok_json(&env.run(&["app", "verify", "--json"]));

    // The Composio nuclear reset leaves the app alone too.
    seed_composio(&env);
    let out = env.run(&["reset", "--confirm", "true"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.file_store().exists("slack", TEAM));
    assert!(env.file_store().exists("slack-app", TEAM));
    ok_json(&env.run(&["app", "verify", "--json"]));
    env.assert_db_has_no_tokens();
}

#[test]
fn manifest_command_prints_the_checked_in_manifest() {
    let env = Env::new();
    let out = env.run(&["app", "manifest"]);
    assert!(out.status.success());
    let on_disk = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/slack-app-manifest.json"),
    )
    .unwrap();
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed, serde_json::from_str::<Value>(&on_disk).unwrap());
}

#[test]
fn tokens_cannot_be_passed_as_plain_arguments() {
    let env = Env::new();
    for flag in ["--app-token", "--bot-token", "--token"] {
        let out = env
            .cmd(&["app", "install", flag, "placeholder-value"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{flag} must not exist");
    }
}
