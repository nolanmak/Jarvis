//! `augmentagent loops list|stop` (#175).
//!
//! Cross-session control of `claude` CLI processes — addresses #174 where a
//! `/loop` scheduled in one Claude Code session keeps firing into Discord but
//! no other session can list or cancel it (`CronList`/`ScheduleWakeup` state
//! lives in the originating session). Listing + signalling PIDs is the only
//! cross-session control surface available on a single host, so this command
//! makes it a first-class CLI primitive that the Discord `!loops` command
//! (#176) and the dashboard (#C) will reuse.
//!
//! Linux-only by design — walks `/proc` directly. The /proc walk + libc kill
//! are abstracted behind [`ProcSource`] + [`Signaler`] traits so the unit
//! tests don't depend on real PIDs.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Subcommand;
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
/// canned fixture.
pub trait ProcSource {
    fn list(&self) -> Result<Vec<ClaudeProc>>;
    /// PID of the running `augmentagent` process. Used by
    /// `stop --all-but-current` to derive the ancestor PID chain.
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
fn looks_like_claude(argv0: &str) -> bool {
    if argv0.is_empty() {
        return false;
    }
    let basename = argv0.rsplit('/').next().unwrap_or(argv0);
    // Strip trailing whitespace just in case the argv was padded.
    basename.trim_end() == "claude"
}

fn format_cmdline(bytes: &[u8]) -> String {
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

/// Returns `(ppid, start_time_ticks)` parsed from `/proc/<pid>/stat`.
/// The comm field (`(name)`) is parenthesised and may contain spaces, so we
/// split on the *last* `)` rather than naively splitting on whitespace.
fn read_stat_ppid_start(pid: i32) -> Option<(i32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let close = stat.rfind(')')?;
    // After "<pid> (<comm>) " the remaining fields are space-separated.
    // Field indices in `after` (0-based): 0=state, 1=ppid, … 19=starttime.
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
pub trait Signaler {
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

#[derive(Debug, Clone, Subcommand)]
pub enum LoopsOp {
    /// List every running `claude` CLI process on this host.
    List {
        /// Emit one JSON document instead of the human table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Send SIGTERM to a `claude` PID. Escalate with `--force` (SIGKILL after
    /// a 5s grace period) or nuke every claude process except this caller's
    /// ancestor chain with `--all-but-current`.
    Stop {
        /// PID to stop. Required unless `--all-but-current` is set.
        pid: Option<i32>,
        /// Escalate to SIGKILL if the process is still alive 5s after SIGTERM.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Stop every claude PID except those in the caller's parent chain.
        #[arg(long, default_value_t = false)]
        all_but_current: bool,
    },
}

/// Entry point used by `main.rs`.
pub async fn run(op: LoopsOp) -> Result<()> {
    let src = ProcFs::new();
    let signaler = LibcSignaler;
    let code = run_with(&src, &signaler, op, Duration::from_secs(5)).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Testable core. `grace` is the wait between SIGTERM and SIGKILL when
/// `--force` is set; production passes 5s, tests pass `Duration::ZERO`.
pub async fn run_with(
    src: &dyn ProcSource,
    signaler: &dyn Signaler,
    op: LoopsOp,
    grace: Duration,
) -> Result<i32> {
    match op {
        LoopsOp::List { json } => {
            let procs = src.list()?;
            if json {
                let payload =
                    serde_json::to_string_pretty(&serde_json::json!({ "loops": procs }))?;
                println!("{}", payload);
            } else {
                print_table(&procs);
            }
            Ok(0)
        }
        LoopsOp::Stop {
            pid,
            force,
            all_but_current,
        } => {
            if all_but_current {
                let exclude = ancestor_claude_pids(src);
                let procs = src.list()?;
                let targets: Vec<i32> = procs
                    .iter()
                    .map(|p| p.pid)
                    .filter(|p| !exclude.contains(p))
                    .collect();
                if targets.is_empty() {
                    eprintln!("no claude processes to stop (excluding caller chain)");
                    return Ok(0);
                }
                let mut any_failed = false;
                for t in targets {
                    if let Err(e) = stop_one(signaler, t, force, grace).await {
                        eprintln!("stop {}: {}", t, e);
                        any_failed = true;
                    } else {
                        println!("stopped {}", t);
                    }
                }
                return Ok(if any_failed { 1 } else { 0 });
            }
            let Some(pid) = pid else {
                return Err(anyhow!("usage: augmentagent loops stop <PID> [--force] | --all-but-current"));
            };
            match stop_one(signaler, pid, force, grace).await {
                Ok(()) => {
                    println!("stopped {}", pid);
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("stop {}: {}", pid, e);
                    Ok(1)
                }
            }
        }
    }
}

/// SIGTERM, optionally wait `grace` and escalate to SIGKILL.
async fn stop_one(
    signaler: &dyn Signaler,
    pid: i32,
    force: bool,
    grace: Duration,
) -> Result<()> {
    if let Err(e) = signaler.term(pid) {
        if e.raw_os_error() == Some(libc::ESRCH) {
            return Err(anyhow!("no such process"));
        }
        return Err(anyhow!("SIGTERM failed: {}", e));
    }
    if !force {
        return Ok(());
    }
    // Poll until grace elapses or the process exits.
    let start = std::time::Instant::now();
    let poll = Duration::from_millis(100);
    while start.elapsed() < grace {
        if !signaler.alive(pid) {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
    if signaler.alive(pid) {
        if let Err(e) = signaler.kill(pid) {
            return Err(anyhow!("SIGKILL failed: {}", e));
        }
    }
    Ok(())
}

/// Walk the ppid chain from `self_pid()` collecting every PID along the way
/// whose argv0 matches `claude`. The caller wants to spare the claude
/// session that invoked `augmentagent` — its session, plus any nested claude
/// process between it and us, must not be stopped.
fn ancestor_claude_pids(src: &dyn ProcSource) -> HashSet<i32> {
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

fn print_table(procs: &[ClaudeProc]) {
    if procs.is_empty() {
        println!("(no claude processes found)");
        return;
    }
    println!(
        "{:>8}  {:>8}  {:>10}  {:<40}  {}",
        "PID", "PPID", "ELAPSED", "CWD", "CMDLINE"
    );
    for p in procs {
        let cwd = p
            .cwd
            .as_deref()
            .map(|c| c.display().to_string())
            .unwrap_or_else(|| "?".to_string());
        let cwd = truncate(&cwd, 40);
        let cmd = truncate(&p.cmdline, 80);
        println!(
            "{:>8}  {:>8}  {:>10}  {:<40}  {}",
            p.pid,
            p.ppid,
            format_elapsed(p.elapsed_secs),
            cwd,
            cmd
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86400, (secs % 86400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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

    struct FakeSig {
        sent: RefCell<Vec<(i32, &'static str)>>,
        alive: RefCell<std::collections::HashSet<i32>>,
        /// PIDs to report ESRCH on for `term`.
        missing: std::collections::HashSet<i32>,
    }
    impl FakeSig {
        fn new(alive: &[i32]) -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
                alive: RefCell::new(alive.iter().copied().collect()),
                missing: std::collections::HashSet::new(),
            }
        }
        fn with_missing(mut self, missing: &[i32]) -> Self {
            self.missing = missing.iter().copied().collect();
            self
        }
        fn die(&self, pid: i32) {
            self.alive.borrow_mut().remove(&pid);
        }
    }
    impl Signaler for FakeSig {
        fn term(&self, pid: i32) -> std::io::Result<()> {
            if self.missing.contains(&pid) {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            self.sent.borrow_mut().push((pid, "TERM"));
            // TERM alone doesn't kill in the fake — the test drives that
            // explicitly via `die()` to model whether the process responds.
            Ok(())
        }
        fn kill(&self, pid: i32) -> std::io::Result<()> {
            self.sent.borrow_mut().push((pid, "KILL"));
            self.die(pid);
            Ok(())
        }
        fn alive(&self, pid: i32) -> bool {
            self.alive.borrow().contains(&pid)
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
        assert!(looks_like_claude("/home/x/.local/share/claude/versions/2.1/claude"));
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
        assert!(out.chars().count() <= 201, "got {} chars", out.chars().count());
        assert!(out.ends_with('…'));
    }

    #[test]
    fn format_elapsed_buckets() {
        assert_eq!(format_elapsed(5), "5s");
        assert_eq!(format_elapsed(75), "1m15s");
        assert_eq!(format_elapsed(3661), "1h01m");
        assert_eq!(format_elapsed(86400 * 2 + 3600 * 3), "2d03h");
    }

    #[tokio::test]
    async fn list_json_emits_loops_array() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1), make_proc(200, 100)],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        // Capture stdout via a helper isn't trivial without external crates;
        // we just assert run_with returns Ok(0) and rely on the JSON
        // serialisation being covered by serde's own tests.
        let code = run_with(
            &fake,
            &sig,
            LoopsOp::List { json: true },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn stop_missing_pid_returns_non_zero_exit() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]).with_missing(&[12345]);
        let code = run_with(
            &fake,
            &sig,
            LoopsOp::Stop {
                pid: Some(12345),
                force: false,
                all_but_current: false,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(code, 1);
    }

    #[tokio::test]
    async fn stop_sends_sigterm_then_returns() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1)],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[100]);
        let code = run_with(
            &fake,
            &sig,
            LoopsOp::Stop {
                pid: Some(100),
                force: false,
                all_but_current: false,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        let sent = sig.sent.borrow().clone();
        assert_eq!(sent, vec![(100, "TERM")]);
    }

    #[tokio::test]
    async fn stop_force_escalates_to_sigkill_when_still_alive() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1)],
            self_pid: 999,
            parents: Default::default(),
        };
        // Process refuses to die from TERM in the fake — the escalation
        // logic must follow up with KILL after the (zero-length) grace.
        let sig = FakeSig::new(&[100]);
        let code = run_with(
            &fake,
            &sig,
            LoopsOp::Stop {
                pid: Some(100),
                force: true,
                all_but_current: false,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        let sent = sig.sent.borrow().clone();
        assert_eq!(sent, vec![(100, "TERM"), (100, "KILL")]);
    }

    #[tokio::test]
    async fn stop_force_no_kill_when_term_succeeds() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1)],
            self_pid: 999,
            parents: Default::default(),
        };
        // Process exits on its own immediately after TERM. We model this by
        // starting it not-alive — `alive()` returns false on the first poll
        // and we never escalate.
        let sig = FakeSig::new(&[]);
        let code = run_with(
            &fake,
            &sig,
            LoopsOp::Stop {
                pid: Some(100),
                force: true,
                all_but_current: false,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        let sent = sig.sent.borrow().clone();
        assert_eq!(sent, vec![(100, "TERM")]);
    }

    #[tokio::test]
    async fn all_but_current_excludes_ancestor_claude() {
        // PID 100 is a running claude process and the parent of bash (200);
        // bash is the parent of augmentagent (self=999). The ancestor walk
        // should pick up 100 and exclude it from the kill set.
        let mut parents = std::collections::HashMap::new();
        parents.insert(999, 200);
        parents.insert(200, 100);
        parents.insert(100, 1);
        let fake = FakeProc {
            procs: vec![
                make_proc(100, 1),   // caller's claude — preserve
                make_proc(300, 1),   // orphaned claude — target
                make_proc(400, 1),   // another orphan — target
            ],
            self_pid: 999,
            parents,
        };
        let sig = FakeSig::new(&[100, 300, 400]);
        let code = run_with(
            &fake,
            &sig,
            LoopsOp::Stop {
                pid: None,
                force: false,
                all_but_current: true,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(code, 0);
        let mut sent: Vec<i32> = sig.sent.borrow().iter().map(|(p, _)| *p).collect();
        sent.sort();
        assert_eq!(sent, vec![300, 400]);
    }

    #[tokio::test]
    async fn stop_without_pid_and_without_all_but_current_errors() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        let err = run_with(
            &fake,
            &sig,
            LoopsOp::Stop {
                pid: None,
                force: false,
                all_but_current: false,
            },
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("usage"));
    }
}
