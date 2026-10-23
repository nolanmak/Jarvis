//! `processes` on Slack (Discord's `!loops`): list and stop the `claude`
//! CLI processes on this host through the shared walker in
//! `augmentagent-loops` (`/proc` on Linux, `ps`/`lsof` on macOS; nothing
//! here reads `/proc`). A walker that cannot run on this host is reported
//! as unavailable, never as an empty list.

use augmentagent_loops::{stop_one, targets_excluding_ancestors, ClaudeProc};

use super::{usage_of, ProcessControl};

pub(super) async fn command(control: &ProcessControl, args: &str) -> String {
    let mut words = args.split_whitespace();
    match words.next().map(str::to_ascii_lowercase).as_deref() {
        None | Some("list") if args.split_whitespace().count() <= 1 => list(control),
        Some("stop") => stop(control, words.collect()).await,
        _ => usage_of("processes"),
    }
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what} is unavailable on this host: {e}")
}

fn list(control: &ProcessControl) -> String {
    match control.source.list() {
        Ok(procs) => render(&procs),
        Err(e) => unavailable("Process listing", e),
    }
}

async fn stop(control: &ProcessControl, words: Vec<&str>) -> String {
    let mut force = false;
    let mut all = false;
    let mut pid: Option<i32> = None;
    for word in words {
        match word {
            "--force" => force = true,
            "--all" | "--all-but-current" => all = true,
            other => match other.parse::<i32>() {
                Ok(n) if n > 1 && pid.is_none() => pid = Some(n),
                _ => return usage_of("processes"),
            },
        }
    }
    if all {
        let targets = match targets_excluding_ancestors(control.source.as_ref()) {
            Ok(t) => t,
            Err(e) => return unavailable("Process control", e),
        };
        if targets.is_empty() {
            return "No `claude` processes to stop (this daemon's own ancestor chain is spared)."
                .into();
        }
        let mut lines = vec![format!("Stopping {} claude process(es):", targets.len())];
        for target in targets {
            let outcome = stop_one(control.signaler.as_ref(), target, force, control.grace).await;
            lines.push(format!("• `{target}` — {outcome}"));
        }
        return lines.join("\n");
    }
    let Some(pid) = pid else {
        return usage_of("processes");
    };
    let outcome = stop_one(control.signaler.as_ref(), pid, force, control.grace).await;
    format!("`{pid}` — {outcome}")
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

fn elapsed(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{:02}h", s / 86400, (s % 86400) / 3600),
    }
}

fn render(procs: &[ClaudeProc]) -> String {
    if procs.is_empty() {
        return "No `claude` processes are running on this host.".into();
    }
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
        let line = format!(
            "{:>8}  {:>8}  {:>10}  {:<32}  {}\n",
            p.pid,
            p.ppid,
            elapsed(p.elapsed_secs),
            truncate(&cwd, 32),
            truncate(&p.cmdline, 60),
        );
        if s.len() + line.len() > 3500 {
            s.push_str("… (truncated)\n");
            break;
        }
        s.push_str(&line);
    }
    s.push_str("```\n`processes stop <pid>` stops one (`--force` for SIGKILL after a grace).");
    s
}
