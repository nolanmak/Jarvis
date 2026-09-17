//! The daemon's private state directory, resolved in one place (#1048).
//!
//! `$XDG_STATE_HOME/augmentagent` when `XDG_STATE_HOME` is an absolute path,
//! else `$HOME/.local/state/augmentagent`; per the XDG spec an empty or
//! relative value is ignored. No systemd unit sets `XDG_STATE_HOME`, so the
//! running daemon keeps its existing paths.
//!
//! Shell and Node readers of the same state, and what cannot follow the rule:
//! - `scripts/lib/service-restart.sh` (`augmentagent_state_dir`) applies this
//!   exact rule to the self-improve lane locks and its restart stamps.
//! - `check-for-updates.sh` and the `install-*.sh` scripts use
//!   `${XDG_STATE_HOME:-$HOME/.local/state}/augmentagent`: the same result
//!   except for a relative value, which they would use and this ignores.
//! - systemd `StandardOutput`/`StandardError` paths (the daemon's `stdout.log`
//!   and `stderr.log`, which `autopr-health` reads from [`state_dir`]) are
//!   fixed when the unit is written: `install-autostart.sh` expands
//!   `XDG_STATE_HOME` at install time, and `scripts/systemd/*.service`
//!   hardcode `%h/.local/state` or an absolute home path. Changing
//!   `XDG_STATE_HOME` later does not move them.
//! - The legacy Node dashboard (`src/dashboard.ts`) reads
//!   `$HOME/.local/state/augmentagent/tool-audit.log` unless
//!   `AUGMENTAGENT_TOOL_AUDIT_LOG` is set; it ignores `XDG_STATE_HOME`.
//!
//! Every state file the Rust code resolves on its own derives from here: the
//! cooldown latch, handoff journals, review history, tool-audit and
//! token-usage logs, memory-nudge cycles and the auto-PR ledgers. Pointing
//! `XDG_STATE_HOME` at a scratch dir therefore isolates a test run from the
//! owner's live state while `HOME`, and with it the real `~/.codex` and
//! `~/.claude` logins that live tests need, stays untouched. Per-file
//! overrides such as `AUGMENTAGENT_COOLDOWN_FILE` still take precedence.
use std::ffi::OsString;
use std::path::PathBuf;

/// The one override honoured by every state path.
pub const STATE_HOME_ENV: &str = "XDG_STATE_HOME";

/// Pure resolution rule, separated from the environment so it can be pinned.
pub fn resolve(state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = state_home.map(PathBuf::from).filter(|dir| dir.is_absolute()) {
        return Some(dir.join("augmentagent"));
    }
    home.map(|home| PathBuf::from(home).join(".local/state/augmentagent"))
}

/// The state directory for this process, or `None` without `HOME` or an
/// absolute `XDG_STATE_HOME`. Callers keep their own HOME-less fallback.
///
/// In this crate's own unit-test binary, and only while `XDG_STATE_HOME` is
/// unset, it returns a private per-process scratch dir instead of
/// `$HOME/.local/state/augmentagent`; that covers the handoff and Codex live
/// tests in `src/`. When `XDG_STATE_HOME` is set, those tests use it as
/// given. Integration tests (`tests/`) and other crates link the normal
/// build, so their live tests rely on [`isolate_for_tests`].
pub fn state_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if std::env::var_os(STATE_HOME_ENV).is_none() {
        return Some(unit_test_state_dir());
    }
    resolve(std::env::var_os(STATE_HOME_ENV), std::env::var_os("HOME"))
}

/// [`state_dir`], else the HOME-relative layout under `home_fallback`; for
/// callers that must always produce a path.
pub fn state_dir_or(home_fallback: &str) -> PathBuf {
    state_dir().unwrap_or_else(|| PathBuf::from(home_fallback).join(".local/state/augmentagent"))
}

#[cfg(test)]
fn unit_test_state_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| scratch("augmentagent-unit-state-").join("augmentagent")).clone()
}

/// A private (0700) scratch directory that lives until the process exits,
/// then is removed so repeated test runs do not accumulate directories.
fn scratch(prefix: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    static CREATED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    static REGISTER: std::sync::Once = std::sync::Once::new();
    extern "C" fn remove_created() {
        // try_lock: never block process exit on a thread that is mid-push.
        if let Ok(created) = CREATED.try_lock() {
            for dir in created.iter() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("create a private scratch state directory")
        .keep();
    CREATED.lock().unwrap_or_else(|e| e.into_inner()).push(dir.clone());
    REGISTER.call_once(|| {
        // SAFETY: registers a plain `extern "C"` function with no captures.
        unsafe { libc::atexit(remove_created) };
    });
    dir
}

/// Every state path this crate resolves without a caller-supplied location.
fn channel_core_state_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        crate::cooldown::CooldownLatch::system().path().to_path_buf(),
        crate::tool_audit::default_audit_log_path(),
        crate::token_usage::default_usage_log_path(),
        crate::memory_nudge::default_cycles_root(),
    ];
    paths.extend(crate::handoff::system_root());
    paths
}

/// Isolate a live or lifecycle test process from the owner's state (#1048).
///
/// Call it first in every opt-in test outside this crate, before any
/// reasoner, logger or latch exists (the global loggers resolve their path
/// once). Unless `XDG_STATE_HOME` already names a directory other than the
/// real one, it points `XDG_STATE_HOME` at a fresh private scratch dir. It
/// then panics if any state path still resolves inside the real state dir,
/// which only happens when a per-file override such as
/// `AUGMENTAGENT_TOOL_AUDIT_LOG` points there. Returns the isolated dir.
#[doc(hidden)]
pub fn isolate_for_tests() -> PathBuf {
    static ISOLATED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ISOLATED
        .get_or_init(|| {
            let real = resolve(None, std::env::var_os("HOME"));
            let configured = resolve(std::env::var_os(STATE_HOME_ENV), None);
            if configured.is_none() || configured == real {
                std::env::set_var(STATE_HOME_ENV, scratch("augmentagent-test-state-"));
            }
            let dir = state_dir().expect("an absolute XDG_STATE_HOME was just set");
            if let Some(real) = &real {
                for path in channel_core_state_paths() {
                    assert!(
                        !path.starts_with(real),
                        "{} still resolves inside the real state dir {}; unset its per-file override",
                        path.display(),
                        real.display()
                    );
                }
            }
            dir
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Env vars that would otherwise route one file around the override.
    const PER_FILE_OVERRIDES: [&str; 3] = [
        "AUGMENTAGENT_COOLDOWN_FILE",
        "AUGMENTAGENT_TOOL_AUDIT_LOG",
        "AUGMENTAGENT_TOKEN_USAGE_LOG",
    ];

    #[test]
    fn resolution_prefers_an_absolute_state_home_and_falls_back_to_home() {
        let home = || Some(OsString::from("/synthetic/home"));
        assert_eq!(
            resolve(Some("/synthetic/scratch".into()), home()),
            Some(PathBuf::from("/synthetic/scratch/augmentagent"))
        );
        let fallback = Some(PathBuf::from("/synthetic/home/.local/state/augmentagent"));
        assert_eq!(resolve(None, home()), fallback);
        assert_eq!(resolve(Some("".into()), home()), fallback, "empty is unset");
        assert_eq!(resolve(Some("relative".into()), home()), fallback, "XDG ignores relative");
        assert_eq!(resolve(None, None), None);
    }

    #[test]
    fn this_crates_tests_never_resolve_the_real_state_dir() {
        let real = resolve(None, std::env::var_os("HOME")).unwrap();
        for path in channel_core_state_paths() {
            assert!(!path.starts_with(&real), "{} is live state", path.display());
        }
    }

    /// Re-run one test in a child process with a controlled environment, so
    /// no env mutation races the tests running in parallel here.
    fn in_child(test: &str, configure: impl FnOnce(&mut std::process::Command)) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env("AUGMENTAGENT_STATE_DIR_CHILD", "1");
        for key in PER_FILE_OVERRIDES {
            child.env_remove(key);
        }
        configure(&mut child);
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child did not run {test}"
        );
    }

    fn is_child() -> bool {
        std::env::var_os("AUGMENTAGENT_STATE_DIR_CHILD").is_some()
    }

    /// C2 pin: with only `XDG_STATE_HOME` set, every state path resolves
    /// under it and real writes through the production paths (latch, review
    /// history, handoff journal, audit and usage logs) land there. `HOME` is a
    /// synthetic home whose state dir must stay absent.
    #[tokio::test]
    async fn every_state_path_and_write_follows_the_state_home_override() {
        if !is_child() {
            let fixture = tempfile::tempdir().unwrap();
            let home = fixture.path().join("home");
            std::fs::create_dir(&home).unwrap();
            in_child("state_dir::tests::every_state_path_and_write_follows_the_state_home_override", |child| {
                child.env("HOME", &home).env(STATE_HOME_ENV, fixture.path().join("scratch"));
            });
            assert!(!home.join(".local").exists(), "a write reached HOME's state dir");
            assert!(fixture.path().join("scratch/augmentagent").is_dir());
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let state = PathBuf::from(std::env::var_os(STATE_HOME_ENV).unwrap()).join("augmentagent");
        assert_eq!(state_dir(), Some(state.clone()));
        assert_eq!(crate::handoff::system_root(), Some(state.join("reasoner-handoffs")));
        assert_eq!(
            crate::cooldown::CooldownLatch::system().path(),
            state.join("reasoner-cooldowns.json").as_path()
        );
        assert_eq!(crate::tool_audit::default_audit_log_path(), state.join("tool-audit.log"));
        assert_eq!(crate::token_usage::default_usage_log_path(), state.join("token-usage.jsonl"));
        assert_eq!(crate::memory_nudge::default_cycles_root(), state);

        crate::cooldown::CooldownLatch::system().latch(
            "claude",
            chrono::Utc::now() + chrono::Duration::minutes(5),
            "synthetic quota",
        );
        let repository = tempfile::tempdir().unwrap();
        crate::fallback::FallbackReasoner::claude_only()
            .track_review_history(repository.path(), "synthetic-branch", false)
            .unwrap();
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.session_id = Some("synthetic-turn".into());
        let journal = crate::handoff::request_path(&crate::handoff::system_root().unwrap(), &opts).unwrap();
        crate::tool_audit::AuditLogger::global()
            .record(&crate::tool_audit::AuditRecord {
                provider: Some("codex".into()),
                ts: "2026-01-01T00:00:00Z".into(),
                session_id: "synthetic-session".into(),
                tool: "Read".into(),
                args: serde_json::json!({}),
                exit_code: None,
                stdout_truncated: None,
                stderr_truncated: None,
                runner: None,
            })
            .await;
        crate::token_usage::UsageLogger::global().append(&crate::token_usage::UsageRecord {
            ts: "2026-01-01T00:00:00Z".into(),
            provider: "codex".into(),
            model: "synthetic".into(),
            class: String::new(),
            usage: Default::default(),
            duration_ms: 0,
        });

        assert!(state.join("reasoner-cooldowns.json").is_file());
        assert!(state.join("review-history").is_dir());
        assert!(journal.starts_with(state.join("reasoner-handoffs").canonicalize().unwrap()));
        assert!(state.join("tool-audit.log").is_file());
        assert!(state.join("token-usage.jsonl").is_file());
        assert!(!home.join(".local").exists(), "a write reached HOME's state dir");
    }

    /// The harness helper replaces a missing or real-dir `XDG_STATE_HOME`
    /// with a private scratch dir and keeps an explicit scratch one.
    #[test]
    fn isolate_for_tests_moves_state_off_the_real_dir() {
        if !is_child() {
            let fixture = tempfile::tempdir().unwrap();
            let home = fixture.path().join("home");
            std::fs::create_dir(&home).unwrap();
            let name = "state_dir::tests::isolate_for_tests_moves_state_off_the_real_dir";
            // Unset, and explicitly pointed at the real state dir: both replaced.
            in_child(name, |child| {
                child.env("HOME", &home).env("TMPDIR", fixture.path())
                    .env_remove(STATE_HOME_ENV).env("EXPECT", "scratch");
            });
            in_child(name, |child| {
                child.env("HOME", &home).env("TMPDIR", fixture.path())
                    .env(STATE_HOME_ENV, home.join(".local/state")).env("EXPECT", "scratch");
            });
            let explicit = fixture.path().join("explicit");
            in_child(name, |child| {
                child.env("HOME", &home).env("TMPDIR", fixture.path())
                    .env(STATE_HOME_ENV, &explicit).env("EXPECT", "explicit");
            });
            assert!(!home.join(".local/state/augmentagent").exists());
            let leftovers: Vec<_> = std::fs::read_dir(fixture.path()).unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("augmentagent-test-state-"))
                .collect();
            assert!(leftovers.is_empty(), "scratch dirs must be removed at exit: {leftovers:?}");
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let before = std::env::var_os(STATE_HOME_ENV);
        let dir = isolate_for_tests();
        assert!(!dir.starts_with(&home), "{}", dir.display());
        assert_eq!(Some(dir.clone()), state_dir());
        if std::env::var("EXPECT").unwrap() == "explicit" {
            assert_eq!(dir, Path::new(&before.unwrap()).join("augmentagent"));
        } else {
            assert!(dir.file_name().is_some_and(|name| name == "augmentagent"));
            let scratch = std::fs::metadata(dir.parent().unwrap()).unwrap();
            let mode = std::os::unix::fs::PermissionsExt::mode(&scratch.permissions());
            assert_eq!(mode & 0o077, 0, "scratch state must be owner-private");
            assert_ne!(std::env::var_os(STATE_HOME_ENV), before);
        }
        for path in channel_core_state_paths() {
            assert!(path.starts_with(&dir), "{} escaped {}", path.display(), dir.display());
        }
        assert_eq!(isolate_for_tests(), dir, "idempotent per process");
    }

    /// Structural pin: no crate resolves `~/.local/state/augmentagent` by
    /// hand. A new state file must go through [`state_dir`], or a test run
    /// with `XDG_STATE_HOME` set would still write the owner's live state.
    #[test]
    fn no_state_path_bypasses_the_shared_resolver() {
        // augmentagent-tools cannot link this crate (it stays out of the
        // daemon's dependency graph); tone-eval mirrors `resolve` inline.
        const EXEMPT: [&str; 2] = [
            "augmentagent-channel-core/src/state_dir.rs",
            "augmentagent-tools/src/bin/tone-eval.rs",
        ];
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut offenders = Vec::new();
        let mut stack = vec![crates.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name != "target") {
                        stack.push(path);
                    }
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let relative = path.strip_prefix(crates).unwrap().to_string_lossy().into_owned();
                if EXEMPT.contains(&relative.as_str()) {
                    continue;
                }
                for (number, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
                    if line.contains(".local/state/augmentagent") && !line.trim_start().starts_with("//") {
                        offenders.push(format!("{relative}:{}", number + 1));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "resolve state paths via state_dir::state_dir(): {offenders:#?}");
    }
}
