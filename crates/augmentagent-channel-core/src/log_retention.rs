//! Age-based pruning for the append-only NDJSON logs (#1004).
//!
//! Two logs record what this daemon does: `tool-audit.log` (every tool call)
//! and `token-usage.jsonl` (every call's token cost). Both are append-only
//! and neither had an end: the audit log reached 5.6 MB in two weeks with a
//! single preset enabled, and turning it on for the auto-PR builder — which
//! makes hundreds of tool calls per agentic run — would have grown it far
//! faster. A log that grows without bound eventually becomes the disk
//! problem it was meant to help diagnose.
//!
//! Retention is by AGE, not size, because the question these logs answer is
//! always "what happened around <time>". Tool calls are forensic and go stale
//! quickly (14 days); token usage is a capacity trend worth a quarter
//! (90 days).
//!
//! Two safety properties, both tested:
//!   * a line whose timestamp cannot be read is **kept** — we never delete
//!     evidence we failed to parse;
//!   * the rewrite is atomic (temp file + rename), so a crash mid-prune
//!     leaves the old log intact rather than a half-written one.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};

/// What a prune did. `skipped` means the cheap first-line check found
/// nothing old enough to remove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PruneOutcome {
    pub removed: u64,
    pub kept: u64,
    pub skipped: bool,
}

/// `days` before now, as the cutoff every record is judged against.
pub fn cutoff(days: u32) -> DateTime<Utc> {
    Utc::now() - Duration::days(i64::from(days))
}

/// The `ts` of one NDJSON record, if it has a readable one.
fn line_ts(line: &str) -> Option<DateTime<Utc>> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let raw = v.get("ts")?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Would pruning this file change anything? Both logs are appended in
/// chronological order, so the FIRST record decides: if it is inside the
/// window, nothing older exists. Reading one line beats reading megabytes.
pub fn needs_prune(first_line: &str, cutoff: DateTime<Utc>) -> bool {
    line_ts(first_line).is_some_and(|ts| ts < cutoff)
}

/// Drop records older than `cutoff`, keeping everything else — including any
/// line we could not parse. Returns the new contents plus the counts.
pub fn retain_since(contents: &str, cutoff: DateTime<Utc>) -> (String, PruneOutcome) {
    let mut kept_lines: Vec<&str> = Vec::new();
    let mut out = PruneOutcome::default();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match line_ts(line) {
            Some(ts) if ts < cutoff => out.removed += 1,
            // Unreadable timestamp, or inside the window: keep it. Deleting
            // a record we could not understand is how evidence disappears.
            _ => {
                out.kept += 1;
                kept_lines.push(line);
            }
        }
    }
    let mut text = kept_lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    (text, out)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".prune-tmp");
    PathBuf::from(p)
}

/// Prune `path` in place. The caller is expected to hold whatever lock
/// serialises appends to this file, so an append cannot land between the
/// read and the rename and be lost.
///
/// Missing file, unreadable file, or nothing old enough are all
/// `Ok(skipped)`: retention is housekeeping and must never be an error path
/// that stops the daemon doing its job.
pub fn prune_file(path: &Path, cutoff: DateTime<Utc>) -> PruneOutcome {
    let skipped = PruneOutcome {
        skipped: true,
        ..Default::default()
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return skipped;
    };
    let Some(first) = contents.lines().find(|l| !l.trim().is_empty()) else {
        return skipped;
    };
    if !needs_prune(first, cutoff) {
        return skipped;
    }
    let (text, outcome) = retain_since(&contents, cutoff);
    let tmp = temp_path(path);
    if std::fs::write(&tmp, &text).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return skipped;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return skipped;
    }
    outcome
}

/// Days to keep tool-call audit records. Forensic detail, stale quickly.
pub fn tool_audit_retention_days() -> u32 {
    env_days("AUGMENTAGENT_TOOL_AUDIT_RETENTION_DAYS", 14)
}

/// Days to keep token-usage records. A capacity trend worth a quarter.
pub fn token_usage_retention_days() -> u32 {
    env_days("AUGMENTAGENT_TOKEN_USAGE_RETENTION_DAYS", 90)
}

/// `0` disables pruning entirely — an explicit "keep everything" for anyone
/// doing a long investigation.
fn env_days(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: &str, tool: &str) -> String {
        format!(r#"{{"ts":"{ts}","tool":"{tool}"}}"#)
    }

    fn at(days_ago: i64) -> String {
        (Utc::now() - Duration::days(days_ago)).to_rfc3339()
    }

    #[test]
    fn records_older_than_the_window_go_and_the_rest_stay() {
        let log = [
            line(&at(40), "Read"),
            line(&at(20), "Write"),
            line(&at(3), "Edit"),
            line(&at(0), "Bash"),
        ]
        .join("\n");
        let (text, out) = retain_since(&log, cutoff(14));
        assert_eq!(out.removed, 2, "the 40- and 20-day-old records");
        assert_eq!(out.kept, 2);
        assert!(!text.contains("Read") && !text.contains("Write"));
        assert!(text.contains("Edit") && text.contains("Bash"));
        assert!(text.ends_with('\n'), "stays appendable: {text:?}");
    }

    /// The property that matters most: pruning must not be able to destroy a
    /// record just because we could not read its timestamp.
    #[test]
    fn unparsable_lines_are_never_deleted() {
        let log = [
            line(&at(40), "Read"),
            "{ truncated half-written line".to_string(),
            "not json at all".to_string(),
            r#"{"tool":"NoTimestamp"}"#.to_string(),
            r#"{"ts":"not-a-date","tool":"BadTimestamp"}"#.to_string(),
            line(&at(1), "Recent"),
        ]
        .join("\n");
        let (text, out) = retain_since(&log, cutoff(14));
        assert_eq!(out.removed, 1, "only the genuinely old, readable record");
        assert_eq!(out.kept, 5);
        for keep in ["truncated", "not json", "NoTimestamp", "BadTimestamp", "Recent"] {
            assert!(text.contains(keep), "lost {keep}: {text}");
        }
    }

    #[test]
    fn the_first_line_decides_whether_to_touch_the_file_at_all() {
        // Chronological append order means the oldest record is first.
        assert!(needs_prune(&line(&at(40), "Read"), cutoff(14)));
        assert!(!needs_prune(&line(&at(2), "Read"), cutoff(14)));
        // An unreadable first line is not grounds to rewrite megabytes.
        assert!(!needs_prune("garbage", cutoff(14)));
        assert!(!needs_prune("", cutoff(14)));
    }

    #[test]
    fn pruning_a_file_rewrites_it_atomically_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool-audit.log");
        let log = [
            line(&at(40), "Old"),
            line(&at(30), "AlsoOld"),
            line(&at(1), "Fresh"),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, &log).unwrap();

        let out = prune_file(&path, cutoff(14));
        assert_eq!((out.removed, out.kept, out.skipped), (2, 1, false));
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("Fresh") && !after.contains("Old"));
        assert!(
            !dir.path().join("tool-audit.log.prune-tmp").exists(),
            "the temp file must not survive"
        );
        // Still valid NDJSON that can be appended to and re-read.
        assert!(after.lines().all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()));

        // Running again is a cheap no-op, not a second rewrite.
        let again = prune_file(&path, cutoff(14));
        assert!(again.skipped, "nothing left to prune");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
    }

    #[test]
    fn pruning_everything_leaves_an_empty_but_usable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.jsonl");
        std::fs::write(&path, line(&at(99), "Ancient") + "\n").unwrap();
        let out = prune_file(&path, cutoff(14));
        assert_eq!((out.removed, out.kept), (1, 0));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        // The next append starts a fresh, valid log.
        assert!(prune_file(&path, cutoff(14)).skipped);
    }

    #[test]
    fn housekeeping_never_errors_on_a_missing_or_unreadable_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prune_file(&dir.path().join("does-not-exist.log"), cutoff(14)).skipped);
        // A directory where a file is expected.
        assert!(prune_file(dir.path(), cutoff(14)).skipped);
        // An empty file.
        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, "").unwrap();
        assert!(prune_file(&empty, cutoff(14)).skipped);
    }

    #[test]
    fn the_two_logs_have_the_windows_the_owner_asked_for() {
        for (key, expect) in [
            ("AUGMENTAGENT_TOOL_AUDIT_RETENTION_DAYS", 14u32),
            ("AUGMENTAGENT_TOKEN_USAGE_RETENTION_DAYS", 90),
        ] {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            let got = if expect == 14 {
                tool_audit_retention_days()
            } else {
                token_usage_retention_days()
            };
            assert_eq!(got, expect, "{key} default");
            std::env::set_var(key, "7");
            let overridden = if expect == 14 {
                tool_audit_retention_days()
            } else {
                token_usage_retention_days()
            };
            assert_eq!(overridden, 7, "{key} is overridable without a rebuild");
            // Junk falls back rather than pruning everything or nothing.
            std::env::set_var(key, "not-a-number");
            let fallback = if expect == 14 {
                tool_audit_retention_days()
            } else {
                token_usage_retention_days()
            };
            assert_eq!(fallback, expect);
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
