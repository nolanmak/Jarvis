//! Per-call token accounting (#1001).
//!
//! The daemon runs on the owner's Claude subscription, so "can the auto-PR
//! loop take more issues?" is a capacity question — and until now nothing
//! measured capacity. `reasoner-calls.jsonl` recorded provider/model/ok and
//! no token counts, and it stopped being written in August. Meanwhile the
//! `claude` CLI has been reporting exact usage on every call, in the same
//! `stream-json` output the reasoner already parses, and we threw it away.
//!
//! This captures it: one NDJSON line per call, appended to a log OUTSIDE the
//! repo (`~/.local/state/augmentagent/token-usage.jsonl`), and a rollup that
//! answers "how much per day, by what".
//!
//! Deliberately not a metrics system: no daemon, no retention policy, no
//! sampling. An append-only file the owner can `grep`, `jq`, or delete.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// What one call cost. Cache fields are separate because they are priced and
/// rate-limited differently from fresh input, and because a large
/// `cache_read` next to a small `input` is the signature of prompt caching
/// working — worth seeing rather than summing away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_creation: u64,
    #[serde(default)]
    pub cache_read: u64,
}

impl TokenUsage {
    /// Everything that entered or left the model. The number to watch for
    /// capacity; not a price.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }

    /// True when the provider reported nothing at all — a call we should not
    /// log, rather than log as free.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_creation += other.cache_creation;
        self.cache_read += other.cache_read;
    }
}

/// Pull usage out of one `stream-json` line.
///
/// The CLI reports totals on its terminal `result` event. Shapes seen in the
/// wild: usage at the top level, and nested under `message` on assistant
/// events (per-turn, which would double-count against the result totals — so
/// ONLY the result event is read).
pub fn parse_usage(line: &str) -> Option<TokenUsage> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("result") {
        return None;
    }
    let u = v.get("usage")?;
    let n = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let usage = TokenUsage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_creation: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
    };
    (!usage.is_empty()).then_some(usage)
}

/// One logged call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// RFC3339 UTC, so `sort` and `grep '^{"ts":"2026-09-15'` both work.
    pub ts: String,
    pub provider: String,
    pub model: String,
    /// Capability class of the preset (`TextOnly`, `WriteTools`, …). A free,
    /// already-available proxy for what kind of work this was.
    #[serde(default)]
    pub class: String,
    #[serde(flatten)]
    pub usage: TokenUsage,
    #[serde(default)]
    pub duration_ms: u64,
}

impl UsageRecord {
    pub fn day(&self) -> &str {
        self.ts.get(..10).unwrap_or("")
    }
}

/// `AUGMENTAGENT_TOKEN_USAGE_LOG` override, else the daemon state dir —
/// outside the repo, like the tool audit log, because it is machine-local
/// runtime data and must never be committed.
pub fn default_usage_log_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("AUGMENTAGENT_TOKEN_USAGE_LOG") {
        if !explicit.trim().is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".local/state/augmentagent")
        .join("token-usage.jsonl")
}

/// Append-only NDJSON writer, mirroring `tool_audit::AuditLogger`: cheap to
/// clone, one short-lived lock per write.
#[derive(Clone, Debug)]
pub struct UsageLogger {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
    /// When this process last ran a retention pass (#1004).
    last_prune: Arc<Mutex<Option<std::time::Instant>>>,
    /// Only the process-global daemon log prunes itself; a logger built with
    /// an explicit path is a plain writer (#1004).
    manages_retention: bool,
}

impl UsageLogger {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Arc::new(Mutex::new(())),
            last_prune: Arc::new(Mutex::new(None)),
            manages_retention: false,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Drop records past the retention window, at most once an hour per
    /// process. Cheap in the common case: the first-line check reads a few
    /// bytes and returns. Runs under the append lock so a concurrent write
    /// cannot be lost to the rewrite.
    fn maybe_prune(&self) {
        if !self.manages_retention {
            return;
        }
        let days = crate::log_retention::token_usage_retention_days();
        if days == 0 {
            return;
        }
        {
            let mut last = self.last_prune.lock().unwrap_or_else(|e| e.into_inner());
            let due = last.is_none_or(|t: std::time::Instant| {
                t.elapsed() >= std::time::Duration::from_secs(3600)
            });
            if !due {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let out = crate::log_retention::prune_file(&self.path, crate::log_retention::cutoff(days));
        if out.removed > 0 {
            tracing::info!(
                removed = out.removed,
                kept = out.kept,
                days,
                "token-usage: pruned records past the retention window"
            );
        }
    }

    pub fn global() -> Arc<UsageLogger> {
        static GLOBAL: std::sync::OnceLock<Arc<UsageLogger>> = std::sync::OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| {
            Arc::new(UsageLogger {
                manages_retention: true,
                ..UsageLogger::new(default_usage_log_path())
            })
        }))
    }

    /// Best effort by design: accounting must never fail a call the model
    /// already answered.
    pub fn append(&self, rec: &UsageRecord) {
        use std::io::Write;
        let Ok(line) = serde_json::to_string(rec) else {
            return;
        };
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{line}");
            }
            Err(e) => tracing::debug!("token usage log unavailable: {e}"),
        }
        // #1004 — housekeeping after the record has landed and the append
        // lock is released, so retention can never delay or reorder a write.
        drop(_guard);
        self.maybe_prune();
    }
}

/// Usage for one day, split by the dimensions we can attribute for free.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DayTotals {
    pub day: String,
    pub calls: u64,
    pub usage: TokenUsage,
    /// `(model, calls, total tokens)`, biggest first.
    pub by_model: Vec<(String, u64, u64)>,
}

/// `(calls, totals, per-model (calls, tokens))` while accumulating one day.
type DayAccumulator = (u64, TokenUsage, std::collections::BTreeMap<String, (u64, u64)>);

/// Roll NDJSON records up per day, newest day last. Unparsable lines are
/// skipped: a truncated tail (the file is appended to live) must not lose the
/// rest of the report.
pub fn rollup(ndjson: &str) -> Vec<DayTotals> {
    use std::collections::BTreeMap;
    let mut days: BTreeMap<String, DayAccumulator> = BTreeMap::new();
    for line in ndjson.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<UsageRecord>(line) else {
            continue;
        };
        let day = rec.day().to_string();
        if day.is_empty() {
            continue;
        }
        let entry = days.entry(day).or_default();
        entry.0 += 1;
        entry.1.add(&rec.usage);
        let m = entry.2.entry(rec.model.clone()).or_insert((0, 0));
        m.0 += 1;
        m.1 += rec.usage.total();
    }
    days.into_iter()
        .map(|(day, (calls, usage, models))| {
            let mut by_model: Vec<(String, u64, u64)> = models
                .into_iter()
                .map(|(m, (c, t))| (m, c, t))
                .collect();
            // Biggest consumer first; ties by name so the output is stable.
            by_model.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
            DayTotals {
                day,
                calls,
                usage,
                by_model,
            }
        })
        .collect()
}

/// Human table. Kept here so the CLI stays a thin caller and the formatting
/// is testable.
pub fn format_report(days: &[DayTotals]) -> String {
    if days.is_empty() {
        return "no token usage recorded yet\n".to_string();
    }
    let mut s = String::from("day         calls      input     output  cache_rd      total\n");
    let mut grand = TokenUsage::default();
    let mut calls = 0u64;
    for d in days {
        s.push_str(&format!(
            "{:<10} {:>6} {:>10} {:>10} {:>9} {:>10}\n",
            d.day,
            d.calls,
            d.usage.input,
            d.usage.output,
            d.usage.cache_read,
            d.usage.total()
        ));
        grand.add(&d.usage);
        calls += d.calls;
    }
    s.push_str(&format!(
        "{:<10} {:>6} {:>10} {:>10} {:>9} {:>10}\n",
        "TOTAL",
        calls,
        grand.input,
        grand.output,
        grand.cache_read,
        grand.total()
    ));
    if let Some(last) = days.last() {
        if !last.by_model.is_empty() {
            s.push_str(&format!("\nby model on {}:\n", last.day));
            for (m, c, t) in &last.by_model {
                s.push_str(&format!("  {m:<28} {c:>4} calls  {t:>10} tokens\n"));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the `claude` CLI actually emits on its terminal event.
    const RESULT_LINE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":1200,"output_tokens":340,"cache_creation_input_tokens":800,"cache_read_input_tokens":24000}}"#;

    #[test]
    fn usage_is_read_from_the_result_event_only() {
        let u = parse_usage(RESULT_LINE).expect("result event carries usage");
        assert_eq!(u.input, 1200);
        assert_eq!(u.output, 340);
        assert_eq!(u.cache_creation, 800);
        assert_eq!(u.cache_read, 24_000);
        assert_eq!(u.total(), 26_340);

        // Assistant events carry PER-TURN usage; counting them too would
        // double-bill against the result totals.
        let assistant = r#"{"type":"assistant","message":{"usage":{"input_tokens":999,"output_tokens":5},"content":[]}}"#;
        assert_eq!(parse_usage(assistant), None);

        // Everything else is quietly ignored.
        assert_eq!(parse_usage(r#"{"type":"system","subtype":"init"}"#), None);
        assert_eq!(parse_usage("not json"), None);
        assert_eq!(parse_usage(""), None);
        // A result with no usage block, or an all-zero one, is not a record.
        assert_eq!(parse_usage(r#"{"type":"result","result":"x"}"#), None);
        assert_eq!(
            parse_usage(r#"{"type":"result","usage":{"input_tokens":0,"output_tokens":0}}"#),
            None
        );
        // Missing fields default rather than failing the whole parse.
        let partial = parse_usage(r#"{"type":"result","usage":{"output_tokens":7}}"#).unwrap();
        assert_eq!((partial.input, partial.output), (0, 7));
    }

    fn rec(ts: &str, model: &str, input: u64, output: u64) -> String {
        serde_json::to_string(&UsageRecord {
            ts: ts.into(),
            provider: "claude".into(),
            model: model.into(),
            class: "TextOnly".into(),
            usage: TokenUsage {
                input,
                output,
                cache_creation: 0,
                cache_read: 0,
            },
            duration_ms: 1234,
        })
        .unwrap()
    }

    #[test]
    fn rollup_groups_by_day_and_ranks_models() {
        let log = [
            rec("2026-09-14T01:00:00Z", "claude-opus-5", 100, 10),
            rec("2026-09-14T02:00:00Z", "claude-fable-5", 50, 5),
            rec("2026-09-14T03:00:00Z", "claude-opus-5", 200, 20),
            rec("2026-09-15T01:00:00Z", "gpt-5.6-terra", 7, 1),
        ]
        .join("\n");
        let days = rollup(&log);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].day, "2026-09-14", "oldest day first");
        assert_eq!(days[0].calls, 3);
        assert_eq!(days[0].usage.input, 350);
        assert_eq!(days[0].usage.output, 35);
        assert_eq!(days[0].usage.total(), 385);
        // Biggest consumer first.
        assert_eq!(days[0].by_model[0].0, "claude-opus-5");
        assert_eq!(days[0].by_model[0].1, 2, "two opus calls");
        assert_eq!(days[0].by_model[0].2, 330);
        assert_eq!(days[0].by_model[1].0, "claude-fable-5");
        assert_eq!(days[1].day, "2026-09-15");
        assert_eq!(days[1].calls, 1);
    }

    #[test]
    fn rollup_survives_a_truncated_or_dirty_tail() {
        // The file is appended to while being read; a half-written last line
        // must not cost us the rest of the report.
        let log = format!(
            "{}\n{{\"ts\":\"2026-09-14T04:00:00Z\",\"prov",
            rec("2026-09-14T01:00:00Z", "claude-opus-5", 10, 1)
        );
        let days = rollup(&log);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].calls, 1);
        // Blank lines and junk are skipped, not fatal.
        assert!(rollup("\n\n").is_empty());
        assert!(rollup("garbage\n{}\n").is_empty());
        // A record with no usable timestamp is skipped rather than bucketed
        // under an empty day.
        assert!(rollup(&rec("", "m", 1, 1)).is_empty());
    }

    #[test]
    fn records_round_trip_through_the_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/token-usage.jsonl");
        let logger = UsageLogger::new(path.clone());
        for (ts, model) in [
            ("2026-09-15T01:00:00Z", "claude-opus-5"),
            ("2026-09-15T02:00:00Z", "claude-opus-5"),
        ] {
            logger.append(&serde_json::from_str(&rec(ts, model, 100, 10)).unwrap());
        }
        let back = std::fs::read_to_string(&path).expect("log created, dirs and all");
        let days = rollup(&back);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].calls, 2);
        assert_eq!(days[0].usage.total(), 220);
        // The line shape is flat, so `jq '.input_tokens'`-style greps work.
        let first: serde_json::Value = serde_json::from_str(back.lines().next().unwrap()).unwrap();
        for k in ["ts", "provider", "model", "input", "output", "duration_ms"] {
            assert!(first.get(k).is_some(), "missing {k} in {first}");
        }
    }

    /// #1004 — a logger built with an explicit path must behave like a plain
    /// writer. Retention belongs to the managed daemon log alone; anything
    /// else would make "write it, read it back" untrue for a caller pointing
    /// a logger at a file of their own (and silently ate a fixture in the
    /// tool-audit suite before this distinction existed).
    #[test]
    fn only_the_global_logger_prunes_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("explicit.jsonl");
        let logger = UsageLogger::new(path.clone());
        assert!(!logger.manages_retention);
        // A record far older than any retention window survives.
        logger.append(&serde_json::from_str(&rec("2020-01-01T00:00:00Z", "m", 5, 5)).unwrap());
        logger.append(&serde_json::from_str(&rec("2020-01-02T00:00:00Z", "m", 5, 5)).unwrap());
        let back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(back.lines().count(), 2, "an explicit logger never deletes: {back}");
        assert!(UsageLogger::global().manages_retention, "the daemon log does");
    }

    #[test]
    fn append_never_panics_on_an_unwritable_path() {
        // Accounting must not be able to fail a call the model already
        // answered — a bad path is a debug line, not an error.
        let logger = UsageLogger::new(PathBuf::from("/proc/cannot/write/here.jsonl"));
        logger.append(&serde_json::from_str(&rec("2026-09-15T01:00:00Z", "m", 1, 1)).unwrap());
    }

    #[test]
    fn report_shows_days_a_total_and_the_days_models() {
        let days = rollup(
            &[
                rec("2026-09-14T01:00:00Z", "claude-opus-5", 100, 10),
                rec("2026-09-15T01:00:00Z", "claude-fable-5", 7, 3),
            ]
            .join("\n"),
        );
        let out = format_report(&days);
        assert!(out.contains("2026-09-14") && out.contains("2026-09-15"));
        assert!(out.contains("TOTAL"), "{out}");
        assert!(out.contains("120"), "grand total present: {out}");
        assert!(out.contains("by model on 2026-09-15"), "{out}");
        assert!(out.contains("claude-fable-5"), "{out}");
        assert_eq!(format_report(&[]), "no token usage recorded yet\n");
    }

    #[test]
    fn the_log_path_is_outside_the_repo_and_overridable() {
        let prev = std::env::var("AUGMENTAGENT_TOKEN_USAGE_LOG").ok();
        std::env::remove_var("AUGMENTAGENT_TOKEN_USAGE_LOG");
        let p = default_usage_log_path();
        assert!(
            p.to_string_lossy().contains(".local/state/augmentagent"),
            "must live in the state dir, never the repo: {p:?}"
        );
        assert!(p.to_string_lossy().ends_with("token-usage.jsonl"));
        std::env::set_var("AUGMENTAGENT_TOKEN_USAGE_LOG", "/tmp/aa-usage-test.jsonl");
        assert_eq!(default_usage_log_path(), PathBuf::from("/tmp/aa-usage-test.jsonl"));
        // An empty override falls back rather than writing to "".
        std::env::set_var("AUGMENTAGENT_TOKEN_USAGE_LOG", "  ");
        assert!(default_usage_log_path().to_string_lossy().ends_with("token-usage.jsonl"));
        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_TOKEN_USAGE_LOG", v),
            None => std::env::remove_var("AUGMENTAGENT_TOKEN_USAGE_LOG"),
        }
    }
}
