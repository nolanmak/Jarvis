//! `augmentagent heartbeat status|run-once` through the real binary (#1317).
//!
//! Every run is isolated the way `reasoner_failover_e2e.rs` isolates its
//! rig: scratch cwd (so the repo `.env` is not loaded), cleared env, and
//! scratch state/cooldown paths. The Claude CLI is a stub script, so no
//! provider is ever contacted.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use augmentagent_store::Store;
use serde_json::Value;

struct Rig {
    tmp: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let rig = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(rig.path("wiki")).unwrap();
        rig
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn write_checklist(&self, text: &str) {
        std::fs::write(self.path("wiki/HEARTBEAT.md"), text).unwrap();
    }

    /// A `claude` stand-in that answers every call with `reply`.
    fn fake_claude(&self, reply: &str) -> PathBuf {
        let text = serde_json::to_string(reply).unwrap();
        let script = format!(
            "#!/usr/bin/env bash\ncat >/dev/null\necho call >> \"{calls}\"\ncat <<'EOF'\n\
             {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":{text}}}]}}}}\n\
             {{\"type\":\"result\",\"result\":{text}}}\nEOF\n",
            calls = self.path("claude-calls").display(),
        );
        let path = self.path("fake-claude.sh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn claude_calls(&self) -> usize {
        std::fs::read_to_string(self.path("claude-calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn run(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        cmd.args(["--db", self.path("data.db").to_str().unwrap()])
            .args(["--wiki-dir", self.path("wiki").to_str().unwrap()])
            .arg("heartbeat")
            .args(args)
            .current_dir(self.tmp.path())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.path("home"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
            .env("AUGMENTAGENT_COOLDOWN_FILE", self.path("cooldowns.json"))
            // Nothing may reach a real CLI; a missing binary fails loudly.
            .env("CLAUDE_CLI", self.path("no-such-claude"))
            .stdin(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn augmentagent");
        println!("--- stdout ---\n{}", String::from_utf8_lossy(&out.stdout));
        println!("--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
        out
    }

    fn json(&self, env: &[(&str, &str)], args: &[&str]) -> Value {
        let out = self.run(env, args);
        assert!(out.status.success(), "exit {}", out.status);
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON document")
    }

    fn seed_run(&self, started_at_ms: i64, status: &str) {
        let store = Store::open(self.path("data.db")).unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO heartbeat_runs (started_at_ms, finished_at_ms, status) VALUES (?1, ?1, ?2)",
                    augmentagent_store::rusqlite::params![started_at_ms, status],
                )
            })
            .unwrap();
    }
}

const ENABLED: (&str, &str) = ("AUGMENTAGENT_HEARTBEAT_ENABLED", "1");

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("exited normally")
}

fn path_str(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn status_on_a_fresh_database_reports_disabled_and_no_runs() {
    let rig = Rig::new();
    let status = rig.json(&[], &["status", "--json"]);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["interval_secs"], 1800);
    assert_eq!(status["last_run"], Value::Null);
    assert_eq!(status["runs"], serde_json::json!([]));
    assert_eq!(status["checklist"]["present"], false);
}

#[test]
fn status_reports_config_and_warnings() {
    let rig = Rig::new();
    rig.write_checklist("- watch for flight changes\n");
    let status = rig.json(
        &[
            ENABLED,
            ("AUGMENTAGENT_HEARTBEAT_INTERVAL", "1h"),
            ("AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS", "08:00-22:00"),
            ("AUGMENTAGENT_HEARTBEAT_TZ", "Nowhere/Land"),
        ],
        &["status", "--json"],
    );
    assert_eq!(status["enabled"], true);
    assert_eq!(status["interval_secs"], 3600);
    assert_eq!(status["active_hours"], "08:00-22:00");
    assert_eq!(status["checklist"]["present"], true);
    assert_eq!(status["warnings"].as_array().unwrap().len(), 1);
}

#[test]
fn check_passes_when_disabled_even_with_no_runs() {
    let rig = Rig::new();
    assert_eq!(exit_code(&rig.run(&[], &["status", "--check"])), 0);
}

#[test]
fn check_fails_when_enabled_and_stale_and_passes_when_fresh() {
    let rig = Rig::new();
    let hour = 3_600_000;
    rig.seed_run(now_ms() - 2 * hour, "silent");
    // 30m interval: two hours without an attempt is past the 3x threshold.
    assert_eq!(exit_code(&rig.run(&[ENABLED], &["status", "--check"])), 1);

    rig.seed_run(now_ms() - 60_000, "silent");
    assert_eq!(exit_code(&rig.run(&[ENABLED], &["status", "--check"])), 0);
}

#[test]
fn check_fails_when_enabled_but_never_run() {
    let rig = Rig::new();
    assert_eq!(exit_code(&rig.run(&[ENABLED], &["status", "--check"])), 1);
}

#[test]
fn check_passes_outside_active_hours() {
    let rig = Rig::new();
    rig.seed_run(now_ms() - 48 * 3_600_000, "silent");
    // A zero-width window is never active, so staleness is never an alarm.
    let env = [
        ENABLED,
        ("AUGMENTAGENT_HEARTBEAT_ACTIVE_HOURS", "09:00-09:00"),
    ];
    assert_eq!(exit_code(&rig.run(&env, &["status", "--check"])), 0);
}

#[test]
fn run_once_without_a_checklist_skips_before_any_model_call() {
    let rig = Rig::new();
    let fake = rig.fake_claude(r#"{"notify": false}"#);
    let report = rig.json(&[("CLAUDE_CLI", path_str(&fake))], &["run-once", "--json"]);
    assert_eq!(report["status"], "skipped");
    assert_eq!(report["reason"], "empty-checklist");
    assert_eq!(rig.claude_calls(), 0);

    let status = rig.json(&[], &["status", "--json"]);
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    assert_eq!(status["last_run"]["reason"], "empty-checklist");
}

// Spawning a provider needs the Linux process-tree supervisor; elsewhere the
// reasoner refuses to run it (same gate as `reasoner_failover_e2e.rs`).
#[cfg(target_os = "linux")]
#[test]
fn dry_run_calls_the_model_and_records_nothing() {
    let rig = Rig::new();
    rig.write_checklist("- tell me if a flight changes\n");
    let fake = rig.fake_claude(r#"{"notify": true, "message": "Flight UA12 moved to 6pm"}"#);
    let report = rig.json(
        &[("CLAUDE_CLI", path_str(&fake))],
        &["run-once", "--dry-run", "--force", "--json"],
    );
    assert_eq!(report["status"], "dry-run");
    assert_eq!(report["decision"], "notify");
    assert_eq!(report["message"], "Flight UA12 moved to 6pm");
    assert_eq!(rig.claude_calls(), 1);

    let status = rig.json(&[], &["status", "--json"]);
    assert_eq!(status["runs"], serde_json::json!([]));
}

// Spawning a provider needs the Linux process-tree supervisor; elsewhere the
// reasoner refuses to run it (same gate as `reasoner_failover_e2e.rs`).
#[cfg(target_os = "linux")]
#[test]
fn silent_run_is_recorded() {
    let rig = Rig::new();
    rig.write_checklist("- tell me if a flight changes\n");
    let fake = rig.fake_claude("HEARTBEAT_OK");
    let report = rig.json(
        &[("CLAUDE_CLI", path_str(&fake))],
        &["run-once", "--force", "--json"],
    );
    assert_eq!(report["status"], "silent");
    let status = rig.json(&[], &["status", "--json"]);
    assert_eq!(status["last_run"]["status"], "silent");
}

#[test]
fn run_once_needs_a_wiki_dir() {
    let rig = Rig::new();
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .args([
            "--db",
            rig.path("data.db").to_str().unwrap(),
            "heartbeat",
            "run-once",
        ])
        .current_dir(rig.tmp.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", rig.path("home"))
        .env("XDG_STATE_HOME", rig.path("state"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--wiki-dir"));
}
