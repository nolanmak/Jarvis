//! `!loops` Discord command (#176) — list + stop running `claude` CLI
//! processes from within a Discord DM with the bot.
//!
//! Distinct from the in-process `/loop` scheduler in [`crate::loops`]: that
//! one runs *user-defined* scheduled tasks the bot owns. `!loops` here is a
//! cross-session escape hatch for nuking the Claude Code CLI sessions
//! themselves when a `/loop` orphans (#174) and there's no terminal handy.
//!
//! Owner-gated: the caller has already passed the bot's `allowed_user_id`
//! allowlist in [`crate::event_handler`] before this is reached. If no
//! allowlist is configured we refuse the command — the alternative is
//! letting any Discord DM-er kill processes on the host, which is
//! unacceptable.

use std::time::Duration;

use augmentagent_loops::{
    stop_one, targets_excluding_ancestors, ClaudeProc, LibcSignaler, ProcFs, ProcSource, Signaler,
};

/// Parse `text` and run the requested op. Returns a Discord-ready reply.
/// `allowlist_active` is the bool form of `event_handler::Handler.state
/// .allowed_user_id.is_some()`; without an allowlist the command refuses.
pub async fn handle(text: &str, allowlist_active: bool) -> String {
    if !allowlist_active {
        return "`!loops` is disabled: bot owner allowlist is not configured. \
                Set `DISCORD_ALLOWED_USER_ID` before using this command."
            .into();
    }
    let src = ProcFs::new();
    let sig = LibcSignaler;
    handle_with(&src, &sig, text, Duration::from_secs(5)).await
}

/// Testable core. Tests inject a fake [`ProcSource`] and [`Signaler`] and
/// pass `Duration::ZERO` for the SIGKILL grace.
pub async fn handle_with(
    src: &dyn ProcSource,
    signaler: &dyn Signaler,
    text: &str,
    grace: Duration,
) -> String {
    let tail = text.trim().strip_prefix("!loops").unwrap_or("").trim();

    // `!loops` with no args → list.
    if tail.is_empty() {
        return match src.list() {
            Ok(procs) => render_list(&procs),
            Err(e) => format!("`!loops` failed: {}", e),
        };
    }

    let mut parts = tail.split_whitespace();
    let verb = parts.next().unwrap_or("");
    if verb != "stop" {
        return format!(
            "unknown `!loops` subcommand `{}`. usage:\n\
             • `!loops` — list claude processes\n\
             • `!loops stop <PID>` — SIGTERM a PID (add `--force` for SIGKILL after 5s)\n\
             • `!loops stop --all` — stop every claude except this bot's ancestor chain",
            verb
        );
    }

    let mut force = false;
    let mut all = false;
    let mut pid: Option<i32> = None;
    for tok in parts {
        match tok {
            "--force" => force = true,
            "--all" | "--all-but-current" => all = true,
            _ => match tok.parse::<i32>() {
                Ok(n) => pid = Some(n),
                Err(_) => {
                    return format!("`!loops stop`: unrecognised argument `{}`", tok);
                }
            },
        }
    }

    if all {
        let targets = match targets_excluding_ancestors(src) {
            Ok(t) => t,
            Err(e) => return format!("`!loops stop --all` failed: {}", e),
        };
        if targets.is_empty() {
            return "no claude processes to stop (excluding bot's ancestor chain)".into();
        }
        let mut lines = Vec::with_capacity(targets.len() + 1);
        lines.push(format!("Stopping {} claude process(es):", targets.len()));
        for t in targets {
            let out = stop_one(signaler, t, force, grace).await;
            lines.push(format!("• `{}` — {}", t, out));
        }
        return lines.join("\n");
    }

    let Some(pid) = pid else {
        return "usage: `!loops stop <PID>` or `!loops stop --all`".into();
    };
    let out = stop_one(signaler, pid, force, grace).await;
    format!("`{}` — {}", pid, out)
}

fn render_list(procs: &[ClaudeProc]) -> String {
    if procs.is_empty() {
        return "no `claude` processes running.".into();
    }
    // Discord renders triple-backtick blocks in a fixed-width font; pad
    // columns so things line up. Cap message length so we don't trip the
    // 2000-char per-message limit even with ~25 long entries.
    let mut s = String::from("```\n");
    s.push_str(&format!(
        "{:>8}  {:>8}  {:>10}  {:<32}  {}\n",
        "PID", "PPID", "ELAPSED", "CWD", "CMDLINE"
    ));
    for p in procs {
        let cwd = p
            .cwd
            .as_deref()
            .map(|c| c.display().to_string())
            .unwrap_or_else(|| "?".into());
        let cwd = truncate(&cwd, 32);
        let cmd = truncate(&p.cmdline, 60);
        let line = format!(
            "{:>8}  {:>8}  {:>10}  {:<32}  {}\n",
            p.pid,
            p.ppid,
            format_elapsed(p.elapsed_secs),
            cwd,
            cmd,
        );
        // Leave room for the closing ``` fence + footer.
        if s.len() + line.len() > 1900 {
            s.push_str("… (truncated)\n");
            break;
        }
        s.push_str(&line);
    }
    s.push_str("```");
    s
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
            elapsed_secs: 75,
            cwd: Some(PathBuf::from("/home/x/AugmentAgent")),
            cmdline: "claude --dangerously-skip-permissions --remote-control".into(),
        }
    }

    #[tokio::test]
    async fn refuses_when_no_allowlist() {
        let reply = handle("!loops", false).await;
        assert!(reply.contains("disabled"));
        assert!(reply.contains("allowlist"));
    }

    #[tokio::test]
    async fn list_renders_table_with_pids() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1), make_proc(200, 100)],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        let reply = handle_with(&fake, &sig, "!loops", Duration::ZERO).await;
        assert!(reply.starts_with("```"));
        assert!(reply.contains("100"));
        assert!(reply.contains("200"));
        assert!(reply.contains("CMDLINE"));
        assert!(reply.ends_with("```"));
    }

    #[tokio::test]
    async fn list_when_empty_says_none() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        let reply = handle_with(&fake, &sig, "!loops", Duration::ZERO).await;
        assert!(reply.contains("no `claude` processes"));
    }

    #[tokio::test]
    async fn stop_pid_succeeds() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1)],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[100]);
        let reply = handle_with(&fake, &sig, "!loops stop 100", Duration::ZERO).await;
        assert!(reply.contains("100"));
        assert!(reply.contains("stopped"));
        assert_eq!(sig.sent_vec(), vec![(100, "TERM")]);
    }

    #[tokio::test]
    async fn stop_force_escalates() {
        let fake = FakeProc {
            procs: vec![make_proc(100, 1)],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[100]);
        let reply = handle_with(&fake, &sig, "!loops stop 100 --force", Duration::ZERO).await;
        assert!(reply.contains("stopped"));
        assert_eq!(sig.sent_vec(), vec![(100, "TERM"), (100, "KILL")]);
    }

    #[tokio::test]
    async fn stop_missing_pid_reports_no_such_process() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]).with_missing(&[12345]);
        let reply = handle_with(&fake, &sig, "!loops stop 12345", Duration::ZERO).await;
        assert!(reply.contains("no such process"));
    }

    #[tokio::test]
    async fn stop_all_excludes_ancestor_chain() {
        let mut parents = std::collections::HashMap::new();
        parents.insert(999, 200);
        parents.insert(200, 100);
        parents.insert(100, 1);
        let fake = FakeProc {
            procs: vec![make_proc(100, 1), make_proc(300, 1), make_proc(400, 1)],
            self_pid: 999,
            parents,
        };
        let sig = FakeSig::new(&[100, 300, 400]);
        let reply = handle_with(&fake, &sig, "!loops stop --all", Duration::ZERO).await;
        assert!(reply.contains("Stopping 2"));
        let mut sent: Vec<i32> = sig.sent_vec().into_iter().map(|(p, _)| p).collect();
        sent.sort();
        assert_eq!(sent, vec![300, 400]);
    }

    #[tokio::test]
    async fn stop_all_when_only_ancestor_present() {
        let mut parents = std::collections::HashMap::new();
        parents.insert(999, 100);
        parents.insert(100, 1);
        let fake = FakeProc {
            procs: vec![make_proc(100, 1)],
            self_pid: 999,
            parents,
        };
        let sig = FakeSig::new(&[100]);
        let reply = handle_with(&fake, &sig, "!loops stop --all", Duration::ZERO).await;
        assert!(reply.contains("no claude processes to stop"));
    }

    #[tokio::test]
    async fn stop_without_pid_or_all_errors() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        let reply = handle_with(&fake, &sig, "!loops stop", Duration::ZERO).await;
        assert!(reply.contains("usage"));
    }

    #[tokio::test]
    async fn unknown_subcommand_shows_usage() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        let reply = handle_with(&fake, &sig, "!loops nuke", Duration::ZERO).await;
        assert!(reply.contains("unknown"));
        assert!(reply.contains("usage"));
    }

    #[tokio::test]
    async fn bad_arg_to_stop_errors_cleanly() {
        let fake = FakeProc {
            procs: vec![],
            self_pid: 999,
            parents: Default::default(),
        };
        let sig = FakeSig::new(&[]);
        let reply = handle_with(&fake, &sig, "!loops stop foobar", Duration::ZERO).await;
        assert!(reply.contains("unrecognised"));
    }
}
