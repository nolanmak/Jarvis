//! Cross-session `claude` CLI process discovery + signalling.
//!
//! Underpins the orphaned-`/loop` fix (#174): a wakeup scheduled in one
//! Claude Code session keeps firing into Discord but no other session can
//! see or cancel it (`CronList`/`ScheduleWakeup`/`TaskList` are
//! per-session). PID-level control is the only cross-session lever
//! available on a single host, and this crate is the shared primitive
//! behind both `augmentagent loops list|stop` (#175) and the Discord
//! `!loops` command (#176).
//!
//! Linux-only by design — walks `/proc` directly.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Serialize;

/// One running `claude` CLI process discovered by [`ProcSource::list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaudeProc {
    pub pid: i32,
    pub ppid: i32,
    /// Seconds since process start, sampled at list time.
    pub elapsed_secs: u64,
    /// `/proc/<pid>/cwd` readlink. `None` if the symlink cannot be resolved
    /// (permission denied for another user's process, or process exited
    /// during the walk).
    pub cwd: Option<PathBuf>,
    /// argv joined with single spaces. Truncated at 200 chars for display.
    pub cmdline: String,
}

/// Read-only view over `/proc`, abstracted so unit tests can substitute a
/// canned fixture. `Send + Sync` so callers can hold a `&dyn ProcSource`
/// across an `await` inside a `tokio::spawn` (the Discord handler does
/// exactly that).
pub trait ProcSource: Send + Sync {
    fn list(&self) -> Result<Vec<ClaudeProc>>;
    /// PID of the running process. Used by `stop --all-but-current` to
    /// derive the ancestor PID chain.
    fn self_pid(&self) -> i32;
    /// Parent of `pid`. `None` when the process no longer exists.
    fn parent_of(&self, pid: i32) -> Option<i32>;
}

/// Real `/proc` walker. Linux-only.
pub struct ProcFs;

impl Default for ProcFs {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcFs {
    pub fn new() -> Self {
        Self
    }
}

impl ProcSource for ProcFs {
    fn list(&self) -> Result<Vec<ClaudeProc>> {
        let uptime = read_uptime_secs();
        let clk_tck = clock_ticks_per_sec();

        let entries = std::fs::read_dir("/proc")?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let Ok(pid) = name_str.parse::<i32>() else {
                continue;
            };
            // Read argv via /proc/<pid>/cmdline (null-separated).
            let cmdline_bytes = match std::fs::read(format!("/proc/{}/cmdline", pid)) {
                Ok(b) => b,
                Err(_) => continue, // process exited mid-walk; skip
            };
            if cmdline_bytes.is_empty() {
                continue; // kernel thread
            }
            let argv0_end = cmdline_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(cmdline_bytes.len());
            let argv0 = std::str::from_utf8(&cmdline_bytes[..argv0_end]).unwrap_or("");
            if !looks_like_claude(argv0) {
                continue;
            }
            let cmdline = format_cmdline(&cmdline_bytes);
            let (ppid, start_ticks) = read_stat_ppid_start(pid).unwrap_or((0, 0));
            let elapsed_secs = if uptime > 0.0 && clk_tck > 0.0 && start_ticks > 0 {
                (uptime - (start_ticks as f64) / clk_tck).max(0.0) as u64
            } else {
                0
            };
            let cwd = std::fs::read_link(format!("/proc/{}/cwd", pid)).ok();
            out.push(ClaudeProc {
                pid,
                ppid,
                elapsed_secs,
                cwd,
                cmdline,
            });
        }
        out.sort_by_key(|p| p.pid);
        Ok(out)
    }

    fn self_pid(&self) -> i32 {
        std::process::id() as i32
    }

    fn parent_of(&self, pid: i32) -> Option<i32> {
        read_stat_ppid_start(pid).map(|(p, _)| p)
    }
}

/// Issue's matching rule: argv0 ends in `/claude` OR matches `^claude($|\s)`.
/// Concretely: take the basename of argv0 and require it to equal "claude".
/// Skips false-positives like `/opt/google/chrome` (matched only because the
/// page URL happens to contain "claude") that a plain substring search would
/// catch.
pub fn looks_like_claude(argv0: &str) -> bool {
    if argv0.is_empty() {
        return false;
    }
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    basename.trim_end() == "claude"
}

pub fn format_cmdline(bytes: &[u8]) -> String {
    let mut s = String::new();
    let mut first = true;
    for chunk in bytes.split(|&b| b == 0) {
        if chunk.is_empty() {
            continue;
        }
        if !first {
            s.push(' ');
        }
        first = false;
        s.push_str(&String::from_utf8_lossy(chunk));
    }
    const MAX: usize = 200;
    if s.len() > MAX {
        s.truncate(MAX);
        s.push('…');
    }
    s
}

/// Returns `(ppid, start_time_ticks)` parsed from `/proc/<pid>/stat`. The
/// comm field (`(name)`) is parenthesised and may contain spaces, so we
/// split on the *last* `)` rather than naively splitting on whitespace.
fn read_stat_ppid_start(pid: i32) -> Option<(i32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let close = stat.rfind(')')?;
    let after = stat.get(close + 2..)?;
    let mut fields = after.split_whitespace();
    let _state = fields.next()?;
    let ppid: i32 = fields.next()?.parse().ok()?;
    // Skip to field index 19 (starttime). We've consumed state+ppid (indices
    // 0+1); need to advance by 17 more.
    let start: u64 = fields.nth(17)?.parse().ok()?;
    Some((ppid, start))
}

fn read_uptime_secs() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

fn clock_ticks_per_sec() -> f64 {
    // SAFETY: `sysconf` is documented to be thread-safe and has no
    // pre-conditions on its argument constant.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as f64
    } else {
        100.0 // Linux conventional default
    }
}

/// Sends signals to PIDs. Abstracted so tests don't fire real `kill(2)`.
/// `Send + Sync` for the same reason [`ProcSource`] is: held across an
/// `await` inside `tokio::spawn` by the Discord handler.
pub trait Signaler: Send + Sync {
    fn term(&self, pid: i32) -> std::io::Result<()>;
    fn kill(&self, pid: i32) -> std::io::Result<()>;
    fn alive(&self, pid: i32) -> bool;
}

pub struct LibcSignaler;

impl Signaler for LibcSignaler {
    fn term(&self, pid: i32) -> std::io::Result<()> {
        send_signal(pid, libc::SIGTERM)
    }
    fn kill(&self, pid: i32) -> std::io::Result<()> {
        send_signal(pid, libc::SIGKILL)
    }
    fn alive(&self, pid: i32) -> bool {
        // `kill(pid, 0)` returns 0 iff the caller has permission to signal
        // the process and the process exists. ESRCH means "no such process".
        let r = unsafe { libc::kill(pid, 0) };
        r == 0
    }
}

fn send_signal(pid: i32, sig: i32) -> std::io::Result<()> {
    // SAFETY: `kill` is async-signal-safe and has no pre-conditions on its
    // arguments — invalid PIDs simply return an error.
    let r = unsafe { libc::kill(pid, sig) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Outcome of a [`stop_one`] call. Allows callers to render success/failure
/// differently (e.g., the CLI prints to stdout, Discord posts to a channel).
#[derive(Debug)]
pub enum StopOutcome {
    Stopped,
    NoSuchProcess,
    TermFailed(std::io::Error),
    KillFailed(std::io::Error),
}

impl StopOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, StopOutcome::Stopped)
    }
}

impl std::fmt::Display for StopOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopOutcome::Stopped => write!(f, "stopped"),
            StopOutcome::NoSuchProcess => write!(f, "no such process"),
            StopOutcome::TermFailed(e) => write!(f, "SIGTERM failed: {}", e),
            StopOutcome::KillFailed(e) => write!(f, "SIGKILL failed: {}", e),
        }
    }
}

/// SIGTERM, optionally wait `grace` and escalate to SIGKILL.
pub async fn stop_one(
    signaler: &dyn Signaler,
    pid: i32,
    force: bool,
    grace: Duration,
) -> StopOutcome {
    if let Err(e) = signaler.term(pid) {
        if e.raw_os_error() == Some(libc::ESRCH) {
            return StopOutcome::NoSuchProcess;
        }
        return StopOutcome::TermFailed(e);
    }
    if !force {
        return StopOutcome::Stopped;
    }
    let start = std::time::Instant::now();
    let poll = Duration::from_millis(100);
    while start.elapsed() < grace {
        if !signaler.alive(pid) {
            return StopOutcome::Stopped;
        }
        tokio::time::sleep(poll).await;
    }
    if signaler.alive(pid) {
        if let Err(e) = signaler.kill(pid) {
            return StopOutcome::KillFailed(e);
        }
    }
    StopOutcome::Stopped
}

/// Walk the ppid chain from `self_pid()` collecting every PID along the way
/// whose argv0 matches `claude`. The caller wants to spare the claude
/// session that invoked the command — its session, plus any nested claude
/// process between it and us, must not be stopped.
pub fn ancestor_claude_pids(src: &dyn ProcSource) -> HashSet<i32> {
    let claude_pids: HashSet<i32> = src
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.pid)
        .collect();
    let mut out = HashSet::new();
    let mut cur = src.self_pid();
    // Bound the walk so a malformed ppid loop can't hang us.
    for _ in 0..256 {
        if cur <= 1 {
            break;
        }
        if claude_pids.contains(&cur) {
            out.insert(cur);
        }
        match src.parent_of(cur) {
            Some(p) if p != cur => cur = p,
            _ => break,
        }
    }
    out
}

/// Compute the kill targets for `--all-but-current` / `!loops stop --all`:
/// every PID returned by `src.list()` minus those in the caller's ancestor
/// chain.
pub fn targets_excluding_ancestors(src: &dyn ProcSource) -> Result<Vec<i32>> {
    let exclude = ancestor_claude_pids(src);
    let procs = src.list()?;
    Ok(procs
        .into_iter()
        .map(|p| p.pid)
        .filter(|p| !exclude.contains(p))
        .collect())
}

/// Convenience: combines the missing-args error path so CLI + Discord render
/// the same message.
pub fn require_pid_or_all_but_current(pid: Option<i32>, all_but_current: bool) -> Result<()> {
    if pid.is_none() && !all_but_current {
        return Err(anyhow!(
            "usage: stop <PID> [--force] | --all-but-current"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeProc {
        procs: Vec<ClaudeProc>,
        self_pid: i32,
        parents: std::collections::HashMap<i32, i32>,
    }

    impl ProcSource for FakeProc {
        fn list(&self) -> Result<Vec<ClaudeProc>> {
            Ok(self.procs.clone())
        }
        fn self_pid(&self) -> i32 {
            self.self_pid
        }
        fn parent_of(&self, pid: i32) -> Option<i32> {
            self.parents.get(&pid).copied()
        }
    }

    // Mutex (not RefCell) so the fake satisfies the `Send + Sync` bound on
    // [`Signaler`] — production-side callers (the Discord handler) hold a
    // `&dyn Signaler` across a `tokio::spawn`'d await.
    struct FakeSig {
        sent: Mutex<Vec<(i32, &'static str)>>,
        alive: Mutex<std::collections::HashSet<i32>>,
        missing: std::collections::HashSet<i32>,
    }
    impl FakeSig {
        fn new(alive: &[i32]) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                alive: Mutex::new(alive.iter().copied().collect()),
                missing: std::collections::HashSet::new(),
            }
        }
        fn with_missing(mut self, missing: &[i32]) -> Self {
            self.missing = missing.iter().copied().collect();
            self
        }
        fn die(&self, pid: i32) {
            self.alive.lock().unwrap().remove(&pid);
        }
        fn sent_vec(&self) -> Vec<(i32, &'static str)> {
            self.sent.lock().unwrap().clone()
        }
    }
    impl Signaler for FakeSig {
        fn term(&self, pid: i32) -> std::io::Result<()> {
            if self.missing.contains(&pid) {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            self.sent.lock().unwrap().push((pid, "TERM"));
            Ok(())
        }
        fn kill(&self, pid: i32) -> std::io::Result<()> {
            self.sent.lock().unwrap().push((pid, "KILL"));
            self.die(pid);
            Ok(())
        }
        fn alive(&self, pid: i32) -> bool {
            self.alive.lock().unwrap().contains(&pid)
        }
    }

    fn make_proc(pid: i32, ppid: i32) -> ClaudeProc {
        ClaudeProc {
            pid,
            ppid,
            elapsed_secs: 30,
            cwd: Some(PathBuf::from("/home/x")),
            cmdline: "claude --dangerously-skip-permissions".into(),
        }
    }

    #[test]
    fn looks_like_claude_basename() {
        assert!(looks_like_claude("claude"));
        assert!(looks_like_claude("/usr/bin/claude"));
        assert!(looks_like_claude(
            "/home/x/.local/share/claude/versions/2.1/claude"
        ));
        assert!(!looks_like_claude("/opt/google/chrome/chrome"));
        assert!(!looks_like_claude("claude-code"));
        assert!(!looks_like_claude(""));
        assert!(!looks_like_claude("node"));
    }

    #[test]
    fn format_cmdline_joins_null_separated_argv() {
        let argv = b"claude\0--dangerously-skip-permissions\0--remote-control\0";
        let out = format_cmdline(argv);
        assert_eq!(out, "claude --dangerously-skip-permissions --remote-control");
    }

    #[test]
    fn format_cmdline_truncates_at_200_chars() {
        let mut argv = b"claude\0".to_vec();
        argv.extend(vec![b'x'; 250]);
        let out = format_cmdline(&argv);
        assert!(
            out.chars().count() <= 201,
            "got {} chars",
            out.chars().count()
        );
        assert!(out.ends_with('…'));
    }

    #[tokio::test]
    async fn stop_missing_pid_reports_no_such_process() {
        let sig = FakeSig::new(&[]).with_missing(&[12345]);
        let out = stop_one(&sig, 12345, false, Duration::ZERO).await;
        assert!(matches!(out, StopOutcome::NoSuchProcess));
        assert!(!out.is_success());
    }

    #[tokio::test]
    async fn stop_term_succeeds() {
        let sig = FakeSig::new(&[100]);
        let out = stop_one(&sig, 100, false, Duration::ZERO).await;
        assert!(out.is_success());
        assert_eq!(sig.sent_vec(), vec![(100, "TERM")]);
    }

    #[tokio::test]
    async fn stop_force_escalates_to_kill_when_still_alive() {
        let sig = FakeSig::new(&[100]);
        let out = stop_one(&sig, 100, true, Duration::ZERO).await;
        assert!(out.is_success());
        assert_eq!(sig.sent_vec(), vec![(100, "TERM"), (100, "KILL")]);
    }

    #[tokio::test]
    async fn stop_force_no_kill_when_term_succeeds() {
        // Process exits on its own immediately after TERM. We model this by
        // starting it not-alive — `alive()` returns false on the first poll
        // and we never escalate.
        let sig = FakeSig::new(&[]);
        let out = stop_one(&sig, 100, true, Duration::ZERO).await;
        assert!(out.is_success());
        assert_eq!(sig.sent_vec(), vec![(100, "TERM")]);
    }

    #[test]
    fn ancestor_walk_excludes_caller_chain() {
        let mut parents = std::collections::HashMap::new();
        parents.insert(999, 200);
        parents.insert(200, 100);
        parents.insert(100, 1);
        let fake = FakeProc {
            procs: vec![make_proc(100, 1), make_proc(300, 1), make_proc(400, 1)],
            self_pid: 999,
            parents,
        };
        let ancestors = ancestor_claude_pids(&fake);
        assert!(ancestors.contains(&100));
        assert!(!ancestors.contains(&300));
        assert!(!ancestors.contains(&400));
    }

    #[test]
    fn targets_excluding_ancestors_returns_orphans() {
        let mut parents = std::collections::HashMap::new();
        parents.insert(999, 200);
        parents.insert(200, 100);
        parents.insert(100, 1);
        let fake = FakeProc {
            procs: vec![make_proc(100, 1), make_proc(300, 1), make_proc(400, 1)],
            self_pid: 999,
            parents,
        };
        let mut t = targets_excluding_ancestors(&fake).unwrap();
        t.sort();
        assert_eq!(t, vec![300, 400]);
    }

    #[test]
    fn require_pid_or_all_but_current_validates() {
        assert!(require_pid_or_all_but_current(Some(100), false).is_ok());
        assert!(require_pid_or_all_but_current(None, true).is_ok());
        let err = require_pid_or_all_but_current(None, false).unwrap_err();
        assert!(err.to_string().contains("usage"));
    }
}
