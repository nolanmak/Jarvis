//! Health watchdog for the auto-PR loop (#997).
//!
//! Every outage this loop has had was plainly visible in
//! `~/.local/state/augmentagent/stderr.log` — and every one was found because
//! the owner noticed no PRs and asked. The loop cannot report its own death
//! (a wedged reasoner logs nothing; a dead tick task logs nothing), so this
//! runs OUT of process on a timer, reads the same evidence a human would, and
//! says so on Discord.
//!
//! The checks are one per real incident:
//!
//! | Code             | Incident it would have caught                       |
//! |------------------|-----------------------------------------------------|
//! | `daemon-down`    | the unit died or was left stopped                    |
//! | `reasoner-wedged`| 2026-09-04: CLI-gate permit leak, 15 h, no triage    |
//! | `loop-silent`    | a tick task that stopped without the process dying   |
//! | `disk-low`       | 2026-09-05→08: root full → false red main → deadlock |
//! | `red-main-stuck` | 2026-09-07: red verdict held behind a gave-up issue  |
//! | `refusal-loop`   | 2026-09-13/14: PR #987 re-refused daily on Cargo.lock|
//! | `no-progress`    | the catch-all: nothing merged in N days             |
//! | `draft-stale`    | a draft nobody will ever finish                     |
//!
//! Analysis is pure over [`HealthInputs`] so every rule is unit-tested
//! against the shape of the incident it exists for; collection is a thin
//! layer that fills the struct from the box.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// How loud a finding is. `Alert` means the loop is not working right now;
/// `Warn` means it is working but wasting itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warn,
    Alert,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Warn => "WARN",
            Severity::Alert => "ALERT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    /// Stable identifier, so a notifier can dedupe across runs.
    pub code: &'static str,
    pub detail: String,
    /// What a human should do about it, concretely.
    pub fix: String,
}

/// Everything the rules judge. Absent evidence is `None`, which never fires a
/// rule: a missing log is a reason to stay quiet, not to cry wolf.
#[derive(Debug, Clone, Default)]
pub struct HealthInputs {
    pub now_or_epoch: Option<DateTime<Utc>>,
    pub daemon_active: bool,
    /// Last completed poll of a channel whose polling REQUIRES a reasoner
    /// call. This is the liveness signal for the whole LLM path.
    pub last_reasoner_poll: Option<DateTime<Utc>>,
    /// Last line the auto-PR loop itself logged (any outcome, including an
    /// idle "daily cap reached").
    pub last_loop_line: Option<DateTime<Utc>>,
    /// Last commit on `origin/main` authored by anything.
    pub last_merge: Option<DateTime<Utc>>,
    pub free_gb_root: Option<f64>,
    pub free_gb_gate: Option<f64>,
    /// When the cached red-`main` verdict was written, if `main` is currently
    /// recorded red.
    pub red_main_since: Option<DateTime<Utc>>,
    /// `(pr, reason, times seen)` for resume refusals in the scanned window.
    pub repeated_refusals: Vec<(u64, String, u32)>,
    /// Open PR numbers, when they could be listed. A refusal loop only
    /// matters while the PR is still there to be re-refused; once it is
    /// closed the finding is history, not a problem. `None` = unknown, in
    /// which case refusals are judged on the window alone.
    pub open_prs: Option<Vec<u64>>,
    /// `(pr, age in days)` for open agent drafts.
    pub draft_ages_days: Vec<(u64, i64)>,
}

/// Thresholds, so a noisy box can be tuned without a rebuild.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub reasoner_silent_mins: i64,
    pub loop_silent_mins: i64,
    pub free_gb_floor: f64,
    pub no_merge_days: i64,
    pub red_main_hours: i64,
    pub draft_stale_days: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            // The loop polls every 30 min and the mail channel more often, so
            // 45 min of silence from the reasoner path is already abnormal.
            reasoner_silent_mins: 45,
            // A tick is 30 min; three missed ticks is not a slow build.
            loop_silent_mins: 95,
            // A workspace test build needs well over this.
            free_gb_floor: 15.0,
            no_merge_days: 3,
            red_main_hours: 6,
            draft_stale_days: 5,
        }
    }
}

fn mins_since(now: DateTime<Utc>, then: DateTime<Utc>) -> i64 {
    (now - then).num_minutes()
}

/// The rules. Order is severity-then-cause, so the first line of a Discord
/// message is the thing to act on.
pub fn analyze(i: &HealthInputs, t: &Thresholds) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let Some(now) = i.now_or_epoch else {
        return out;
    };

    if !i.daemon_active {
        out.push(Finding {
            severity: Severity::Alert,
            code: "daemon-down",
            detail: "augmentagent.service is not active".into(),
            fix: "systemctl --user start augmentagent.service".into(),
        });
        // Everything below measures a running daemon; don't pile on.
        return out;
    }

    if let Some(last) = i.last_reasoner_poll {
        let age = mins_since(now, last);
        if age >= t.reasoner_silent_mins {
            out.push(Finding {
                severity: Severity::Alert,
                code: "reasoner-wedged",
                detail: format!(
                    "no reasoner-backed poll completed for {age} min (last {}). \
                     Email triage is stopped, not just the loop.",
                    last.format("%Y-%m-%d %H:%MZ")
                ),
                fix: "Check for a leaked CLI-gate permit (no children under the \
                      daemon PID, no watchdog warning), then \
                      `systemctl --user restart augmentagent.service`."
                    .into(),
            });
        }
    }

    if let Some(last) = i.last_loop_line {
        let age = mins_since(now, last);
        if age >= t.loop_silent_mins {
            out.push(Finding {
                severity: Severity::Alert,
                code: "loop-silent",
                detail: format!(
                    "the auto-PR loop has logged nothing for {age} min (last {})",
                    last.format("%Y-%m-%d %H:%MZ")
                ),
                fix: "The tick task may have died while the process lives; \
                      restart the daemon and check for a panic in stderr.log."
                    .into(),
            });
        }
    }

    for (label, free) in [("/", i.free_gb_root), ("the gate target", i.free_gb_gate)] {
        if let Some(gb) = free {
            if gb < t.free_gb_floor {
                out.push(Finding {
                    severity: Severity::Alert,
                    code: "disk-low",
                    detail: format!(
                        "{label} has {gb:.1} GB free, below the {:.0} GB a workspace \
                         test build needs",
                        t.free_gb_floor
                    ),
                    fix: "Free space before the gate runs again — a gate that fails \
                          on a full disk gets cached as a red `main`."
                        .into(),
                });
            }
        }
    }

    if let Some(since) = i.red_main_since {
        let hours = (now - since).num_hours();
        if hours >= t.red_main_hours {
            out.push(Finding {
                severity: Severity::Alert,
                code: "red-main-stuck",
                detail: format!(
                    "`main` has been recorded red for {hours} h; every gate is \
                     failing until it clears"
                ),
                fix: "Verify main by hand (`cargo test -p <the named crate>`). If it \
                      is green, the verdict came from an infrastructure failure: \
                      remove autopr-baseline.json and let the loop re-check."
                    .into(),
            });
        }
    }

    for (pr, reason, times) in &i.repeated_refusals {
        let still_open = i.open_prs.as_ref().is_none_or(|open| open.contains(pr));
        if *times >= 2 && still_open {
            out.push(Finding {
                severity: Severity::Warn,
                code: "refusal-loop",
                detail: format!(
                    "PR #{pr} has been refused {times} times for the same reason \
                     ({reason}); each one can cost a daily slot"
                ),
                fix: "A refusal that repeats is deterministic: close the PR or \
                      remove the cause, rather than letting it recur."
                    .into(),
            });
        }
    }

    if let Some(last) = i.last_merge {
        let days = (now - last).num_days();
        if days >= t.no_merge_days {
            out.push(Finding {
                severity: Severity::Warn,
                code: "no-progress",
                detail: format!(
                    "nothing has merged to main for {days} days (last {})",
                    last.format("%Y-%m-%d")
                ),
                fix: "Read the last few `auto-PR:` lines in stderr.log — the loop \
                      records why each run ended."
                    .into(),
            });
        }
    }

    let stale: Vec<String> = i
        .draft_ages_days
        .iter()
        .filter(|(_, d)| *d >= t.draft_stale_days)
        .map(|(pr, d)| format!("#{pr} ({d}d)"))
        .collect();
    if !stale.is_empty() {
        out.push(Finding {
            severity: Severity::Warn,
            code: "draft-stale",
            detail: format!("agent drafts open with no resolution: {}", stale.join(", ")),
            fix: "Each is a run's work nobody will finish; merge, take over, or close."
                .into(),
        });
    }

    out.sort_by(|a, b| b.severity.cmp(&a.severity));
    out
}

/// One Discord/stdout message for a set of findings. `None` when healthy —
/// a watchdog that pings on every clean run trains you to ignore it.
pub fn report(findings: &[Finding]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let alerts = findings
        .iter()
        .filter(|f| f.severity == Severity::Alert)
        .count();
    let mut s = if alerts > 0 {
        format!("🚨 auto-PR loop unhealthy — {alerts} alert(s)\n")
    } else {
        "⚠️ auto-PR loop is running but wasting itself\n".to_string()
    };
    for f in findings {
        s.push_str(&format!(
            "\n**{} {}** — {}\n↳ {}\n",
            f.severity.as_str(),
            f.code,
            f.detail,
            f.fix
        ));
    }
    Some(s)
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// The last `max` bytes of a file — the daemon log is hundreds of MB and only
/// its tail is ever relevant.
fn tail_bytes(path: &Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Drop ANSI SGR sequences. The daemon writes coloured tracing output, so a
/// raw log line carries escapes in the middle of the text — they broke both
/// timestamp parsing and the refusal grouping until this existed.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI is `ESC [ <params> <final byte in @..~>`. The `[` is itself in
        // that range, so it must be consumed BEFORE scanning for the final
        // byte or the sequence ends on its own introducer.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // A two-character escape: drop both.
            Some(_) => {}
            None => break,
        }
    }
    out
}

/// The daemon's tracing lines begin with an RFC3339 timestamp, possibly
/// behind a colour escape.
pub fn line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    let cleaned = strip_ansi(line);
    let tok = cleaned.split_whitespace().next()?;
    DateTime::parse_from_rfc3339(tok)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Most recent timestamp on a line containing `needle`.
pub fn last_timestamp_with(log: &str, needle: &str) -> Option<DateTime<Utc>> {
    log.lines()
        .rev()
        .find(|l| l.contains(needle))
        .and_then(line_timestamp)
}

/// `(pr, reason, count)` for resume refusals — the signature of a loop that
/// is spending slots to reach the same verdict again and again.
///
/// Only refusals at or after `since` count. The log tail spans days, so an
/// unbounded scan keeps reporting a PR that was closed last week — and a
/// watchdog that alerts on solved problems is one you learn to ignore. A
/// line with no parsable timestamp is skipped for the same reason.
pub fn scan_repeated_refusals(log: &str, since: DateTime<Utc>) -> Vec<(u64, String, u32)> {
    let mut seen: std::collections::BTreeMap<(u64, String), u32> = Default::default();
    for raw in log.lines() {
        let line = strip_ansi(raw);
        match line_timestamp(&line) {
            Some(ts) if ts >= since => {}
            _ => continue,
        }
        let Some(rest) = line.split("PR #").nth(1) else {
            continue;
        };
        let Some((num, tail)) = rest.split_once(": ") else {
            continue;
        };
        let Ok(pr) = num.trim().parse::<u64>() else {
            continue;
        };
        if !(tail.contains("resume refused") || tail.contains("resume skipped")) {
            continue;
        }
        // Normalise away the trailing run counters so identical verdicts group.
        let reason = tail
            .split(" runs_today")
            .next()
            .unwrap_or(tail)
            .trim()
            .trim_end_matches(|c: char| c == ';' || c == ',')
            .to_string();
        *seen.entry((pr, reason)).or_insert(0) += 1;
    }
    seen.into_iter()
        .map(|((pr, reason), n)| (pr, reason, n))
        .collect()
}

fn state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/state/augmentagent"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn free_gb(path: &Path) -> Option<f64> {
    let out = std::process::Command::new("df")
        .args(["-k", "--output=avail"])
        .arg(path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let kb: f64 = text.lines().nth(1)?.trim().parse().ok()?;
    Some(kb / 1024.0 / 1024.0)
}

fn daemon_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "augmentagent.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is `main` currently recorded red, and since when (the cache file's mtime)?
fn red_main_since(dir: &Path) -> Option<DateTime<Utc>> {
    let p = dir.join("autopr-baseline.json");
    let raw = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let failing = v.get("failing").and_then(|f| f.as_array()).map(|a| !a.is_empty());
    let build_err = v.get("gate_err").map(|g| !g.is_null());
    if failing != Some(true) && build_err != Some(true) {
        return None;
    }
    let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(mtime))
}

/// Every open PR number, or `None` when `gh` could not be asked — unknown
/// must not silence a real finding.
fn open_pr_numbers() -> Option<Vec<u64>> {
    let out = std::process::Command::new("gh")
        .args(["pr", "list", "--state", "open", "--limit", "100", "--json", "number"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        v.as_array()?
            .iter()
            .filter_map(|p| p.get("number")?.as_u64())
            .collect(),
    )
}

fn open_draft_ages(now: DateTime<Utc>) -> Vec<(u64, i64)> {
    let out = std::process::Command::new("gh")
        .args([
            "pr", "list", "--state", "open", "--limit", "50", "--json",
            "number,isDraft,headRefName,createdAt",
        ])
        .output()
        .ok();
    let Some(out) = out.filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|p| p.get("isDraft").and_then(|d| d.as_bool()) == Some(true))
                .filter(|p| {
                    p.get("headRefName")
                        .and_then(|h| h.as_str())
                        .is_some_and(|h| h.starts_with("agent-fix/"))
                })
                .filter_map(|p| {
                    let n = p.get("number")?.as_u64()?;
                    let created = p.get("createdAt")?.as_str()?;
                    let dt = DateTime::parse_from_rfc3339(created).ok()?;
                    Some((n, (now - dt.with_timezone(&Utc)).num_days()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn last_merge(repo_root: &Path) -> Option<DateTime<Utc>> {
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%cI", "origin/main"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    DateTime::parse_from_rfc3339(&s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Gather everything the rules need from this box.
pub fn collect(repo_root: &Path) -> HealthInputs {
    let now = Utc::now();
    let dir = state_dir();
    let log = tail_bytes(&dir.join("stderr.log"), 4 * 1024 * 1024).unwrap_or_default();
    let gate_dir = std::env::var("AUGMENTAGENT_GATE_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".cache/augmentagent-gate-target"))
                .unwrap_or_else(|| PathBuf::from("."))
        });

    HealthInputs {
        now_or_epoch: Some(now),
        daemon_active: daemon_active(),
        last_reasoner_poll: last_timestamp_with(&log, r#"poll complete channel="gmail""#),
        last_loop_line: last_timestamp_with(&log, "augmentagent::self_improve"),
        last_merge: last_merge(repo_root),
        free_gb_root: free_gb(Path::new("/")),
        free_gb_gate: gate_dir
            .exists()
            .then(|| free_gb(&gate_dir))
            .flatten(),
        red_main_since: red_main_since(&dir),
        repeated_refusals: scan_repeated_refusals(&log, now - chrono::Duration::days(3)),
        open_prs: open_pr_numbers(),
        draft_ages_days: open_draft_ages(now),
    }
}

/// Entry point for the `autopr-health` subcommand. Exit code 0 when healthy
/// or only warnings, 1 when anything is an alert, so a timer/CI can gate on it.
pub async fn run(repo_root: &Path, notify: bool, json: bool) -> anyhow::Result<i32> {
    let inputs = collect(repo_root);
    let findings = analyze(&inputs, &Thresholds::default());

    if json {
        let payload: Vec<_> = findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "severity": f.severity.as_str(),
                    "code": f.code,
                    "detail": f.detail,
                    "fix": f.fix,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        match report(&findings) {
            Some(text) => println!("{text}"),
            None => println!("✓ auto-PR loop healthy"),
        }
    }

    if notify {
        if let Some(text) = report(&findings) {
            notify_discord(&text).await;
        }
    }

    Ok(i32::from(
        findings.iter().any(|f| f.severity == Severity::Alert),
    ))
}

async fn notify_discord(text: &str) {
    let Ok(url) = std::env::var("DISCORD_WEBHOOK_URL") else {
        return;
    };
    if url.trim().is_empty() {
        return;
    }
    let clipped: String = text.chars().take(1800).collect();
    let body = serde_json::json!({ "content": clipped });
    if let Err(e) = reqwest::Client::new()
        .post(url.trim())
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        tracing::warn!("autopr-health: Discord notify failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A healthy box: recent poll, recent tick, recent merge, plenty of disk.
    fn healthy() -> HealthInputs {
        HealthInputs {
            now_or_epoch: Some(t0()),
            daemon_active: true,
            last_reasoner_poll: Some(t0() - Duration::minutes(4)),
            last_loop_line: Some(t0() - Duration::minutes(12)),
            last_merge: Some(t0() - Duration::hours(20)),
            free_gb_root: Some(70.0),
            free_gb_gate: Some(53.0),
            red_main_since: None,
            repeated_refusals: vec![],
            open_prs: None,
            draft_ages_days: vec![(990, 0)],
        }
    }

    fn codes(f: &[Finding]) -> Vec<&'static str> {
        f.iter().map(|x| x.code).collect()
    }

    #[test]
    fn a_healthy_loop_reports_nothing() {
        let f = analyze(&healthy(), &Thresholds::default());
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(report(&f), None, "a clean run must not ping Discord");
    }

    /// 2026-09-04: the CLI-gate permit leak. The process stayed active and
    /// non-LLM pollers kept logging, so only the reasoner-backed poll going
    /// quiet reveals it.
    #[test]
    fn catches_the_wedged_reasoner() {
        let i = HealthInputs {
            last_reasoner_poll: Some(t0() - Duration::hours(15)),
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(codes(&f), vec!["reasoner-wedged"]);
        assert_eq!(f[0].severity, Severity::Alert);
        assert!(f[0].detail.contains("Email triage is stopped"), "{:?}", f[0]);
        assert!(f[0].fix.contains("restart"), "{:?}", f[0]);
        // Just under the threshold stays quiet.
        let ok = HealthInputs {
            last_reasoner_poll: Some(t0() - Duration::minutes(44)),
            ..healthy()
        };
        assert!(analyze(&ok, &Thresholds::default()).is_empty());
    }

    /// 2026-09-05→08: root filled, the gate failed on it, and the failure was
    /// cached as a red `main`.
    #[test]
    fn catches_a_full_disk_before_the_gate_runs() {
        let i = HealthInputs {
            free_gb_root: Some(0.4),
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(codes(&f), vec!["disk-low"]);
        assert!(f[0].detail.contains("0.4 GB"), "{:?}", f[0]);
        // The gate's own filesystem is checked separately.
        let g = HealthInputs {
            free_gb_gate: Some(2.0),
            ..healthy()
        };
        assert_eq!(codes(&analyze(&g, &Thresholds::default())), vec!["disk-low"]);
    }

    /// 2026-09-07: a red verdict held behind a gave-up issue, forever.
    #[test]
    fn catches_a_red_main_that_never_clears() {
        let i = HealthInputs {
            red_main_since: Some(t0() - Duration::hours(30)),
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(codes(&f), vec!["red-main-stuck"]);
        assert!(f[0].fix.contains("autopr-baseline.json"), "{:?}", f[0]);
        // A verdict minutes old is just the loop working.
        let fresh = HealthInputs {
            red_main_since: Some(t0() - Duration::minutes(20)),
            ..healthy()
        };
        assert!(analyze(&fresh, &Thresholds::default()).is_empty());
    }

    /// 2026-09-13/14: PR #987 refused twice for the same reason, a slot each.
    #[test]
    fn catches_a_refusal_that_repeats() {
        let i = HealthInputs {
            repeated_refusals: vec![
                (987, "resume refused — blast radius on `Cargo.lock`".into(), 2),
                (990, "resume skipped — merge conflict with main".into(), 1),
            ],
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(codes(&f), vec!["refusal-loop"], "only the repeat fires");
        assert!(f[0].detail.contains("#987") && f[0].detail.contains("2 times"));
        assert_eq!(f[0].severity, Severity::Warn);

        // Once the PR is closed the loop cannot recur, so the finding is
        // history — reporting it would train the reader to ignore the alert.
        let closed = HealthInputs {
            open_prs: Some(vec![990]),
            ..i.clone()
        };
        assert!(analyze(&closed, &Thresholds::default()).is_empty());
        // Still open ⇒ still reported.
        let open = HealthInputs {
            open_prs: Some(vec![987, 990]),
            ..i.clone()
        };
        assert_eq!(codes(&analyze(&open, &Thresholds::default())), vec!["refusal-loop"]);
        // `gh` unavailable ⇒ unknown must not silence it.
        let unknown = HealthInputs { open_prs: None, ..i };
        assert_eq!(codes(&analyze(&unknown, &Thresholds::default())), vec!["refusal-loop"]);
    }

    #[test]
    fn catches_no_progress_and_stale_drafts() {
        let i = HealthInputs {
            last_merge: Some(t0() - Duration::days(4)),
            draft_ages_days: vec![(987, 9), (990, 1)],
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(codes(&f), vec!["no-progress", "draft-stale"]);
        assert!(f[1].detail.contains("#987 (9d)"), "{:?}", f[1]);
        assert!(!f[1].detail.contains("#990"), "a fresh draft is not stale");
    }

    #[test]
    fn a_dead_daemon_is_the_only_thing_reported() {
        let i = HealthInputs {
            daemon_active: false,
            last_reasoner_poll: Some(t0() - Duration::hours(20)),
            free_gb_root: Some(0.1),
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(codes(&f), vec!["daemon-down"], "no pile-on once it is down");
        assert!(f[0].fix.contains("systemctl --user start"));
    }

    #[test]
    fn missing_evidence_never_fires_a_rule() {
        // A box with no log, no git, no df readings: stay quiet rather than
        // alert on absence.
        let i = HealthInputs {
            now_or_epoch: Some(t0()),
            daemon_active: true,
            ..Default::default()
        };
        assert!(analyze(&i, &Thresholds::default()).is_empty());
        // No clock at all ⇒ nothing to measure against.
        assert!(analyze(&HealthInputs::default(), &Thresholds::default()).is_empty());
    }

    #[test]
    fn alerts_sort_above_warnings_in_the_message() {
        let i = HealthInputs {
            last_merge: Some(t0() - Duration::days(9)),
            free_gb_root: Some(1.0),
            ..healthy()
        };
        let f = analyze(&i, &Thresholds::default());
        assert_eq!(f[0].severity, Severity::Alert, "alert first");
        let msg = report(&f).expect("findings produce a message");
        assert!(msg.starts_with("🚨"), "{msg}");
        assert!(msg.find("disk-low").unwrap() < msg.find("no-progress").unwrap());
        // Warnings alone get the quieter header.
        let warn_only = analyze(
            &HealthInputs {
                last_merge: Some(t0() - Duration::days(9)),
                ..healthy()
            },
            &Thresholds::default(),
        );
        assert!(report(&warn_only).unwrap().starts_with("⚠️"));
    }

    #[test]
    fn scanning_survives_the_daemons_ansi_colour_codes() {
        // Verbatim shape from stderr.log: escapes sit between the message and
        // the structured fields, so an unstripped scan carries them into the
        // Discord alert and splits identical reasons into separate groups.
        let esc = "\u{1b}[3m";
        let reset = "\u{1b}[0m";
        let log = format!(
            "2026-09-13T00:38:01.1Z  {esc}INFO{reset} augmentagent::self_improve: auto-PR: PR #987: resume refused — blast radius on `Cargo.lock` {esc}runs_today{reset}=1\n\
             2026-09-14T00:27:02.2Z  {esc}INFO{reset} augmentagent::self_improve: auto-PR: PR #987: resume refused — blast radius on `Cargo.lock` {esc}runs_today{reset}=1\n"
        );
        let r = scan_repeated_refusals(&log, t0() - Duration::days(3));
        assert_eq!(r.len(), 1, "the escapes must not split the group: {r:?}");
        assert_eq!(r[0].2, 2);
        assert!(!r[0].1.contains('\u{1b}'), "no escapes in the alert: {:?}", r[0].1);
        assert!(r[0].1.ends_with("`Cargo.lock`"), "{:?}", r[0].1);
        assert!(strip_ansi(&format!("{esc}x{reset}y")) == "xy");
        // A timestamp behind a colour escape still parses.
        assert!(line_timestamp(&format!("{esc}2026-09-14T00:27:02.2Z{reset} INFO x")).is_some());
    }

    /// A refusal that was dealt with must stop being reported once it ages
    /// out of the window, even though the log tail still contains it.
    #[test]
    fn refusal_scan_is_time_bounded() {
        let log = "\
2026-09-01T00:38:01.1Z  INFO augmentagent::self_improve: auto-PR: PR #987: resume refused — blast radius on `Cargo.lock` runs_today=1
2026-09-02T00:27:02.2Z  INFO augmentagent::self_improve: auto-PR: PR #987: resume refused — blast radius on `Cargo.lock` runs_today=1
2026-09-13T00:38:01.1Z  INFO augmentagent::self_improve: auto-PR: PR #991: resume skipped — merge conflict with main runs_today=1
";
        // A three-day window at 2026-09-14 sees only the recent one, which on
        // its own is not yet a loop.
        let recent = scan_repeated_refusals(log, t0() - Duration::days(3));
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].0, 991);
        assert_eq!(recent[0].2, 1, "one refusal is not a repeat");
        assert!(
            analyze(
                &HealthInputs { repeated_refusals: recent, ..healthy() },
                &Thresholds::default()
            )
            .is_empty(),
            "a single refusal must not alert"
        );
        // A wide window still sees the historical pair.
        let all = scan_repeated_refusals(log, t0() - Duration::days(30));
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().find(|r| r.0 == 987).unwrap().2, 2);
        // Lines without a timestamp are never counted.
        assert!(scan_repeated_refusals("PR #987: resume refused — x", t0() - Duration::days(30)).is_empty());
    }

    #[test]
    fn log_scanning_reads_timestamps_and_groups_refusals() {
        let log = "\
2026-09-13T00:38:01.1Z  INFO augmentagent::self_improve: auto-PR: PR #987: resume refused — blast radius on `Cargo.lock` runs_today=1 daily_cap=3
2026-09-13T04:10:00.0Z  INFO augmentagent_channel_core::trigger: poll complete channel=\"gmail\" handled=12
2026-09-14T00:27:02.2Z  INFO augmentagent::self_improve: auto-PR: PR #987: resume refused — blast radius on `Cargo.lock` runs_today=1 daily_cap=3
2026-09-14T01:03:00.0Z  INFO augmentagent::self_improve: auto-PR: PR #988: resumed and MERGED runs_today=2 daily_cap=3
";
        let poll = last_timestamp_with(log, r#"poll complete channel="gmail""#).expect("poll ts");
        assert_eq!(poll.format("%Y-%m-%dT%H:%M").to_string(), "2026-09-13T04:10");
        let loop_line = last_timestamp_with(log, "augmentagent::self_improve").expect("loop ts");
        assert_eq!(loop_line.format("%Y-%m-%dT%H:%M").to_string(), "2026-09-14T01:03");
        assert!(last_timestamp_with(log, "nothing matches this").is_none());

        let refusals = scan_repeated_refusals(log, t0() - Duration::days(3));
        assert_eq!(refusals.len(), 1, "the merge is not a refusal: {refusals:?}");
        let (pr, reason, times) = &refusals[0];
        assert_eq!(*pr, 987);
        assert_eq!(*times, 2, "the run counters must not split the group");
        assert!(reason.contains("Cargo.lock"));
    }
}
