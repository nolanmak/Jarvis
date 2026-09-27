//! Process-shared provider cooldown latch (#655/#659).
//!
//! When a provider refuses on quota (or times out / errors provider-side),
//! the [`FallbackReasoner`](crate::fallback::FallbackReasoner) latches it
//! until a reset instant. The latch is a small JSON file — NOT in-memory
//! state — for two load-bearing reasons:
//!
//! 1. **It must survive daemon restarts.** The auto-updater bounces the
//!    service on every deploy; an in-memory latch would make each restart
//!    rediscover the outage the expensive way (one ~24k-token spawn per
//!    unread email per poll tick, the #448 failure shape).
//! 2. **It must be visible to CLI one-shots.** The digest timer, tone
//!    refresh, and wiki-migrate run as separate processes from the daemon;
//!    they share the file, so one process's refusal backs everyone off.
//!
//! Reads are per-check (the file is tiny; a poll tick does at most a few
//! reads), writes are atomic (`tmp` + rename) so concurrent processes never
//! observe a torn file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// One latched provider: don't spawn it again until `until`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CooldownEntry {
    pub until: DateTime<Utc>,
    /// Human-readable cause, e.g. the quota refusal text. Diagnostic only.
    pub reason: String,
    /// #1040 — consecutive unrecognised failures on write-capable calls, see
    /// [`CooldownLatch::strike`]. Not a latch by itself; an entry holding only
    /// strikes has an `until` in the past. Older binaries ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strikes: Option<Strikes>,
}

/// A run of consecutive unrecognised failures for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Strikes {
    pub count: u32,
    pub last: DateTime<Utc>,
}

/// File-backed latch. Cheap to construct; holds no open handles.
#[derive(Debug, Clone)]
pub struct CooldownLatch {
    path: PathBuf,
}

impl CooldownLatch {
    /// System latch path: `AUGMENTAGENT_COOLDOWN_FILE` override (tests), else
    /// `reasoner-cooldowns.json` in the shared [`state_dir`](crate::state_dir)
    /// (same state dir as the daemon logs), else a cwd-relative fallback so a
    /// HOME-less environment still functions.
    pub fn system() -> Self {
        if let Ok(p) = std::env::var("AUGMENTAGENT_COOLDOWN_FILE") {
            if !p.trim().is_empty() {
                return Self { path: PathBuf::from(p) };
            }
        }
        let path = crate::state_dir::state_dir()
            .map(|dir| dir.join("reasoner-cooldowns.json"))
            .unwrap_or_else(|| PathBuf::from("reasoner-cooldowns.json"));
        Self { path }
    }

    /// Test constructor pinned to an explicit path.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The latch file this instance reads and writes.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn read_all(&self) -> BTreeMap<String, CooldownEntry> {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return BTreeMap::new();
        };
        match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(e) => {
                // A torn/garbled file must never wedge the chain shut or
                // open — treat as empty and let the next write repair it.
                warn!(path = %self.path.display(), "cooldown file unreadable ({e}); ignoring");
                BTreeMap::new()
            }
        }
    }

    fn write_all(&self, map: &BTreeMap<String, CooldownEntry>) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Unique tmp per writer (#655 review): a fixed tmp name let two
        // concurrent processes interleave write/rename and publish a torn
        // file. Read-modify-write lost updates remain possible and are
        // accepted — a lost latch costs one extra probe, a lost clear costs
        // one skipped probe until expiry; a torn file would have wiped ALL
        // latches at once.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let body = match serde_json::to_vec_pretty(map) {
            Ok(b) => b,
            Err(e) => {
                warn!("cooldown serialize failed: {e}");
                return;
            }
        };
        if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &self.path))
        {
            // Best-effort by design: a read-only disk degrades to in-run
            // behavior (each process re-learns the outage) rather than
            // taking the reasoner down.
            warn!(path = %self.path.display(), "cooldown write failed: {e}");
        }
    }

    /// Is `provider` latched right now? Returns the reset instant if so.
    pub fn latched_until(&self, provider: &str) -> Option<DateTime<Utc>> {
        let entry = self.read_all().remove(provider)?;
        if entry.until > Utc::now() {
            Some(entry.until)
        } else {
            None
        }
    }

    /// Like [`latched_until`](Self::latched_until), plus the recorded reason:
    /// `(reset as unix seconds, reason)`. The auto-PR quota brake (#1215)
    /// counts only quota latches, and only the reason tells them apart.
    pub fn latched_entry(&self, provider: &str) -> Option<(i64, String)> {
        let entry = self.read_all().remove(provider)?;
        (entry.until > Utc::now()).then(|| (entry.until.timestamp(), entry.reason))
    }

    /// Latch `provider` until `until`. Later of (existing, new) wins so a
    /// racing shorter latch can't shrink a parsed reset time.
    pub fn latch(&self, provider: &str, until: DateTime<Utc>, reason: &str) {
        let mut map = self.read_all();
        let keep_existing = map
            .get(provider)
            .is_some_and(|e| e.until >= until && e.until > Utc::now());
        if keep_existing {
            return;
        }
        debug!(provider, %until, "latching provider cooldown");
        // Single-line, short reason: this file is durable local state and
        // `reasoner-selftest` prints it — never persist a wall of raw model
        // output or provider stderr here (#655 review).
        let reason: String = reason
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        // A latch starts a new run: earlier strikes are spent.
        map.insert(provider.to_string(), CooldownEntry { until, reason, strikes: None });
        self.write_all(&map);
    }

    /// #1040 — count one more consecutive unrecognised failure for `provider`
    /// and return the run length. A strike is not a latch (`latched_until`
    /// ignores it). The run ends on a success (`clear`), on a latch, or when
    /// the previous strike is older than `window`, so strikes days apart
    /// never add up to a backoff.
    pub fn strike(&self, provider: &str, window: chrono::Duration) -> u32 {
        let now = Utc::now();
        let mut map = self.read_all();
        let entry = map.entry(provider.to_string()).or_insert_with(|| CooldownEntry {
            until: now,
            reason: "unrecognised failures (not latched)".into(),
            strikes: None,
        });
        let count = match entry.strikes {
            Some(run) if now - run.last <= window => run.count.saturating_add(1),
            _ => 1,
        };
        entry.strikes = Some(Strikes { count, last: now });
        self.write_all(&map);
        count
    }

    /// Clear `provider`'s latch (called on a successful call so recovery is
    /// observed immediately rather than waiting out a stale latch). No-op —
    /// and crucially no file write — when the provider isn't latched.
    pub fn clear(&self, provider: &str) {
        let mut map = self.read_all();
        if map.remove(provider).is_some() {
            self.write_all(&map);
        }
    }

    /// All currently-active latches (for status surfaces).
    pub fn active(&self) -> BTreeMap<String, CooldownEntry> {
        let now = Utc::now();
        self.read_all()
            .into_iter()
            .filter(|(_, e)| e.until > now)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latch_persists_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let latch = CooldownLatch::at(dir.path().join("cd.json"));

        assert!(latch.latched_until("claude").is_none());

        let until = Utc::now() + chrono::Duration::minutes(10);
        latch.latch("claude", until, "You've hit your session limit");
        assert_eq!(latch.latched_until("claude"), Some(until));

        // A second handle on the same path (≈ another process) sees it.
        let other = CooldownLatch::at(dir.path().join("cd.json"));
        assert_eq!(other.latched_until("claude"), Some(until));

        // Expired entries read as unlatched.
        latch.latch("gemini", Utc::now() - chrono::Duration::seconds(1), "past");
        assert!(latch.latched_until("gemini").is_none());

        // clear() removes.
        latch.clear("claude");
        assert!(other.latched_until("claude").is_none());
    }

    #[test]
    fn longer_existing_latch_wins() {
        let dir = tempfile::tempdir().unwrap();
        let latch = CooldownLatch::at(dir.path().join("cd.json"));
        let far = Utc::now() + chrono::Duration::hours(2);
        let near = Utc::now() + chrono::Duration::minutes(5);
        latch.latch("claude", far, "parsed reset");
        latch.latch("claude", near, "default cooldown");
        assert_eq!(latch.latched_until("claude"), Some(far));
    }

    #[test]
    fn strikes_count_a_run_that_success_latch_or_age_ends() {
        let dir = tempfile::tempdir().unwrap();
        let latch = CooldownLatch::at(dir.path().join("cd.json"));
        let window = chrono::Duration::hours(1);
        assert_eq!(latch.strike("codex", window), 1);
        assert_eq!(latch.strike("codex", window), 2);
        assert!(latch.latched_until("codex").is_none(), "a strike is not a latch");
        assert!(latch.active().is_empty(), "strike-only entries are not shown as latches");
        latch.clear("codex");
        assert_eq!(latch.strike("codex", window), 1, "a success ends the run");
        latch.latch("codex", Utc::now() + chrono::Duration::minutes(1), "backoff");
        assert!(latch.latched_until("codex").is_some());
        assert_eq!(latch.strike("codex", window), 1, "a latch ends the run");
        assert_eq!(latch.strike("codex", chrono::Duration::zero() - chrono::Duration::seconds(1)), 1,
            "a run older than the window starts over");
        // Other processes on older binaries still read the file.
        let raw = std::fs::read_to_string(dir.path().join("cd.json")).unwrap();
        #[derive(Deserialize)]
        struct Legacy { #[allow(dead_code)] until: DateTime<Utc>, #[allow(dead_code)] reason: String }
        let legacy: BTreeMap<String, Legacy> = serde_json::from_str(&raw).unwrap();
        assert!(legacy.contains_key("codex"));
    }

    #[test]
    fn garbled_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cd.json");
        std::fs::write(&path, b"{not json").unwrap();
        let latch = CooldownLatch::at(path);
        assert!(latch.latched_until("claude").is_none());
        // And the next write repairs it.
        let until = Utc::now() + chrono::Duration::minutes(1);
        latch.latch("claude", until, "x");
        assert_eq!(latch.latched_until("claude"), Some(until));
    }
}
