//! Deterministic failover e2e for the #655 provider chain (#666).
//!
//! Drives the real `augmentagent reasoner-selftest` subprocess with every
//! provider CLI replaced by a committed stub from
//! `scripts/reasoner-fault-injection/`, so a full quota-refusal → failover →
//! cooldown-latch round trip is exercised against a real built binary
//! (real `build_reasoner`, real adapters, real on-disk latch) without
//! spending a token of anyone's quota.
//!
//! ## Minting a PR-gate receipt
//!
//! Every scenario prints the child's exit status, stdout, and stderr. Run
//! with `--nocapture` and that transcript is the verification receipt
//! `scripts/agent-pr-verify-gate.sh` asks for — see
//! `docs/REASONER-FAULT-INJECTION.md` for the exact recipe and for what a
//! rig receipt does NOT prove.
//!
//! `AUGMENTAGENT_E2E_BIN` points the scenarios at another binary (normally
//! `./target/release/augmentagent`) so the receipt can be minted against
//! the artifact that will actually be deployed.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use chrono::{DateTime, Utc};
use tempfile::TempDir;

/// Binary under test: the freshly-built one by default, overridable so the
/// same scenarios can mint a receipt against `./target/release/augmentagent`.
fn e2e_bin() -> PathBuf {
    match std::env::var("AUGMENTAGENT_E2E_BIN") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_augmentagent")),
    }
}

/// Locate the stub family by walking up from this crate — same convention
/// as the code-mode sidecar lookup.
fn fakes_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let cand = dir.join("scripts/reasoner-fault-injection");
        if cand.join("_lib.sh").is_file() {
            return cand;
        }
        dir.pop();
    }
    panic!(
        "scripts/reasoner-fault-injection not found above {}",
        env!("CARGO_MANIFEST_DIR")
    );
}

struct Rig {
    tmp: TempDir,
    fakes: PathBuf,
}

impl Rig {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        // The fakes keep their invocation counters under $HOME (see
        // `_lib.sh`), so the scratch home doubles as the counter store.
        std::fs::create_dir_all(tmp.path().join("home")).expect("scratch home");
        // `build_reasoner` drops codex from the chain unless it can
        // authenticate; a throwaway CODEX_HOME with an auth.json makes it
        // eligible without reading the owner's real `~/.codex`.
        std::fs::create_dir_all(tmp.path().join("codex-home")).expect("scratch codex home");
        std::fs::write(tmp.path().join("codex-home/auth.json"), "{}").expect("stub auth.json");
        Rig {
            tmp,
            fakes: fakes_dir(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    /// One `reasoner-selftest` invocation, fully isolated from the box it
    /// runs on. `fakes` maps a provider's CLI-override env var to the stub
    /// that should serve it.
    fn cmd(&self, chain: &str, fakes: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(e2e_bin());
        cmd.args([
            "--db",
            self.path("data.db").to_str().expect("utf8 db path"),
            "reasoner-selftest",
        ])
        // `main()` runs `dotenvy::dotenv()` against the cwd: anywhere but a
        // scratch dir and the repo's real `.env` joins the test.
        .current_dir(self.tmp.path())
        // Nothing from the developer's shell (or the daemon's env) may
        // change which providers are eligible or where they are spawned
        // from. PATH stays because the stubs are `#!/usr/bin/env bash`.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("RUST_LOG", "info")
        .env("HOME", self.path("home"))
        .env("AUGMENTAGENT_REASONER_CHAIN", chain)
        // LOAD-BEARING: without this the binary latches
        // `~/.local/state/augmentagent/reasoner-cooldowns.json` and the
        // owner's live daemon stops calling Claude for half an hour.
        .env("AUGMENTAGENT_COOLDOWN_FILE", self.path("cooldowns.json"))
        .env("AUGMENTAGENT_CODEX_HOME", self.path("codex-home"))
        .env("GEMINI_API_KEY", "fake-not-a-key")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        for (var, script) in fakes {
            cmd.env(var, self.fakes.join(script));
        }
        cmd
    }

    /// Run and echo the whole transcript — this printout IS the receipt.
    fn run(&self, label: &str, mut cmd: Command) -> Output {
        let out = cmd.output().expect("spawn augmentagent");
        println!("\n=== {label} (exit {}) ===", out.status);
        println!("--- stdout ---\n{}", String::from_utf8_lossy(&out.stdout));
        println!("--- stderr ---\n{}", String::from_utf8_lossy(&out.stderr));
        out
    }

    /// How many times `provider`'s stub was actually spawned.
    fn spawns(&self, provider: &str) -> usize {
        std::fs::read_to_string(self.path(&format!("home/.fake-cli/{provider}.count")))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// `(until, reason)` from the on-disk latch, or `None` when the provider
    /// is not latched.
    fn latch(&self, provider: &str) -> Option<(DateTime<Utc>, String)> {
        let raw = std::fs::read_to_string(self.path("cooldowns.json")).ok()?;
        let doc: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let entry = doc.get(provider)?;
        let until = entry.get("until")?.as_str()?.parse::<DateTime<Utc>>().ok()?;
        Some((until, entry.get("reason")?.as_str()?.to_string()))
    }
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The headline scenario: the primary refuses on quota (as a *successful*
/// stream — #448's shape), the call is served by the fallback, the primary
/// is latched, and the next call does not spawn it at all.
#[test]
fn quota_refusal_fails_over_to_codex_and_latches() {
    let rig = Rig::new();
    let fakes = [
        ("CLAUDE_CLI", "fake-claude-quota.sh"),
        ("CODEX_CLI", "fake-codex-ok.sh"),
    ];

    let out = rig.run("run 1 — claude refuses on quota", rig.cmd("claude,codex", &fakes));
    assert!(out.status.success(), "selftest should be served by codex");
    let stdout = stdout_of(&out);
    assert!(stdout.contains("chain: claude → codex"), "{stdout}");
    assert!(stdout.contains("response: PONG-FROM-FAKE-CODEX"), "{stdout}");
    assert!(
        stdout.contains("cooldowns (after call): claude latched until"),
        "the selftest must report the latch it just took: {stdout}"
    );

    let (until, reason) = rig.latch("claude").expect("claude must be latched");
    // Never assert the instant: the refusal's reset hint resolves against a
    // wall clock and is clamped by the 6h ceiling.
    assert!(until > Utc::now(), "latch must be in the future: {until}");
    assert!(reason.contains("session limit"), "{reason}");
    assert_eq!(rig.spawns("claude"), 1);
    assert_eq!(rig.spawns("codex"), 1);

    let out2 = rig.run("run 2 — claude latched", rig.cmd("claude,codex", &fakes));
    assert!(out2.status.success(), "{}", stderr_of(&out2));
    assert!(stdout_of(&out2).contains("response: PONG-FROM-FAKE-CODEX"));
    assert_eq!(
        rig.spawns("claude"),
        1,
        "a latched provider must not be spawned again — that skip is the \
         whole anti-amplification property"
    );
    assert_eq!(rig.spawns("codex"), 2);
}

/// A primary that never answers is a typed `Timeout`, not a stuck pipeline:
/// the watchdog kills it, the fallback serves, and the primary is latched.
#[test]
fn hung_primary_times_out_and_fallback_serves() {
    let rig = Rig::new();
    let mut cmd = rig.cmd(
        "claude,codex",
        &[
            ("CLAUDE_CLI", "fake-claude-hang.sh"),
            ("CODEX_CLI", "fake-codex-ok.sh"),
        ],
    );
    cmd.env("AUGMENTAGENT_REASONER_TIMEOUT_SECS", "2");

    let started = std::time::Instant::now();
    let out = rig.run("hung primary", cmd);
    let elapsed = started.elapsed();

    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the watchdog must cut the hung child loose, not wait it out: {elapsed:?}"
    );
    assert!(stdout_of(&out).contains("response: PONG-FROM-FAKE-CODEX"));
    let (_, reason) = rig.latch("claude").expect("a timeout is latchworthy for text-only calls");
    assert!(reason.contains("timed out"), "{reason}");
}

/// When the whole chain refuses, the exit code is non-zero and the error
/// callers see is the PRIMARY's — a trailing fallback failure must not mask
/// the rate limit that actually caused the outage.
#[test]
fn whole_chain_refusing_exits_nonzero_with_primary_error() {
    let rig = Rig::new();
    let out = rig.run(
        "whole chain refusing",
        rig.cmd(
            "claude,codex",
            &[
                ("CLAUDE_CLI", "fake-claude-quota.sh"),
                ("CODEX_CLI", "fake-codex-usage-limit.sh"),
            ],
        ),
    );

    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    let stderr = stderr_of(&out);
    assert!(stderr.contains("selftest FAILED"), "{stderr}");
    assert!(
        stderr.contains("claude rate limit"),
        "the primary's error must survive to the caller: {stderr}"
    );
    assert!(rig.latch("claude").is_some(), "primary latched");
    assert!(rig.latch("codex").is_some(), "fallback latched too");
}

/// Same seam, second fallback adapter: gemini serves text-only calls and
/// latches on a 429 error object.
#[test]
fn gemini_fallback_serves_and_latches() {
    let serving = Rig::new();
    let out = serving.run(
        "gemini serves",
        serving.cmd(
            "claude,gemini",
            &[
                ("CLAUDE_CLI", "fake-claude-quota.sh"),
                ("GEMINI_CLI", "fake-gemini-ok.sh"),
            ],
        ),
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("response: PONG-FROM-FAKE-GEMINI"));
    assert_eq!(serving.spawns("gemini"), 1);

    let exhausted = Rig::new();
    let out = exhausted.run(
        "gemini 429",
        exhausted.cmd(
            "claude,gemini",
            &[
                ("CLAUDE_CLI", "fake-claude-quota.sh"),
                ("GEMINI_CLI", "fake-gemini-429.sh"),
            ],
        ),
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout_of(&out));
    let (_, reason) = exhausted.latch("gemini").expect("gemini must be latched");
    assert!(reason.contains("rate limit"), "{reason}");
}

/// The negative control: a healthy primary means the fallback is never
/// spawned and nothing is latched.
#[test]
fn healthy_primary_never_spawns_fallback() {
    let rig = Rig::new();
    let out = rig.run(
        "healthy primary",
        rig.cmd(
            "claude,codex",
            &[
                ("CLAUDE_CLI", "fake-claude-ok.sh"),
                ("CODEX_CLI", "fake-codex-ok.sh"),
            ],
        ),
    );

    assert!(out.status.success(), "{}", stderr_of(&out));
    let stdout = stdout_of(&out);
    assert!(stdout.contains("response: PONG-FROM-FAKE-CLAUDE"), "{stdout}");
    assert!(stdout.contains("cooldowns (after call): none active"), "{stdout}");
    assert_eq!(rig.spawns("claude"), 1);
    assert_eq!(rig.spawns("codex"), 0, "fallback must not be probed on success");
    assert!(rig.latch("claude").is_none());
}
