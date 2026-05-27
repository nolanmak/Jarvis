//! `augmentagent loops list|stop` (#175).
//!
//! Thin clap wrapper around the [`augmentagent_loops`] primitive — the
//! shared crate carries the `/proc` walker + signal helpers so the Discord
//! `!loops` command (#176) can call the same code without circular deps
//! through the CLI binary.

use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;

use augmentagent_loops::{
    require_pid_or_all_but_current, stop_one, targets_excluding_ancestors, ClaudeProc,
    LibcSignaler, ProcFs, ProcSource,
};

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
    signaler: &dyn augmentagent_loops::Signaler,
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
                let targets = targets_excluding_ancestors(src)?;
                if targets.is_empty() {
                    eprintln!("no claude processes to stop (excluding caller chain)");
                    return Ok(0);
                }
                let mut any_failed = false;
                for t in targets {
                    let out = stop_one(signaler, t, force, grace).await;
                    if out.is_success() {
                        println!("stopped {}", t);
                    } else {
                        eprintln!("stop {}: {}", t, out);
                        any_failed = true;
                    }
                }
                return Ok(if any_failed { 1 } else { 0 });
            }
            require_pid_or_all_but_current(pid, all_but_current)?;
            let pid = pid.expect("require_pid_or_all_but_current rejected None above");
            let out = stop_one(signaler, pid, force, grace).await;
            if out.is_success() {
                println!("stopped {}", pid);
                Ok(0)
            } else {
                eprintln!("stop {}: {}", pid, out);
                Ok(1)
            }
        }
    }
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
    use augmentagent_loops::Signaler;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FakeProc {
        procs: Vec<ClaudeProc>,
        self_pid: i32,
        parents: std::collections::HashMap<i32, i32>,
    }

    impl ProcSource for FakeProc {
        fn list(&self) -> anyhow::Result<Vec<ClaudeProc>> {
            Ok(self.procs.clone())
        }
        fn self_pid(&self) -> i32 {
            self.self_pid
        }
        fn parent_of(&self, pid: i32) -> Option<i32> {
            self.parents.get(&pid).copied()
        }
    }

    // `Mutex` (not `RefCell`) so the fake satisfies the `Send + Sync` bound
    // on the trait — required because the Discord handler holds a `&dyn
    // Signaler` across an await inside `tokio::spawn`.
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
            self.alive.lock().unwrap().remove(&pid);
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
        let code = run_with(&fake, &sig, LoopsOp::List { json: true }, Duration::ZERO)
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
    async fn all_but_current_stops_orphans() {
        let mut parents = std::collections::HashMap::new();
        parents.insert(999, 200);
        parents.insert(200, 100);
        parents.insert(100, 1);
        let fake = FakeProc {
            procs: vec![
                make_proc(100, 1), // caller's claude — preserve
                make_proc(300, 1), // orphan — target
                make_proc(400, 1), // orphan — target
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
        let mut sent: Vec<i32> = sig.sent_vec().into_iter().map(|(p, _)| p).collect();
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
