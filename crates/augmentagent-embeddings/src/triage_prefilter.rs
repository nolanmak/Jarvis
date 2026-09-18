//! Similarity pre-filter for routine mail (#1127).
//!
//! The daemon's past triage decisions are the reasoner's own, not a human's
//! (there is no human-labelled triage corpus). So this measures and preserves
//! **agreement with the reasoner**: when a new message's nearest already-
//! triaged neighbours are unanimous `skip`, close, and clearly separated
//! from any non-skip neighbour, the message is skipped without a model call.
//! Anything else falls through to normal triage. Only `skip` is ever
//! emitted.
//!
//! Vectors come from the same `message_vectors` store (an email is one
//! chunk). Eligible neighbours: live-triaged rows on eligible platforms with
//! a reasoner decision, excluding rows the pre-filter itself decided.

use std::collections::HashMap;
use std::sync::RwLock;

use augmentagent_store::rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::embedder::dot;
use crate::vectors::from_blob;

pub const ENV_ENABLED: &str = "AUGMENTAGENT_TRIAGE_PREFILTER";
pub const ENV_MIN_SIM: &str = "AUGMENTAGENT_TRIAGE_PREFILTER_MIN_SIM";
pub const ENV_MIN_MARGIN: &str = "AUGMENTAGENT_TRIAGE_PREFILTER_MIN_MARGIN";
pub const ENV_K: &str = "AUGMENTAGENT_TRIAGE_PREFILTER_K";
pub const ENV_SPOT_RATE: &str = "AUGMENTAGENT_TRIAGE_PREFILTER_SPOT_PCT";

/// Platforms whose rows were live-triaged by the reasoner. History importers
/// stamp `digest_only` without triage and are never eligible.
pub const ELIGIBLE_PLATFORMS: &[&str] = &["gmail"];
pub const ELIGIBLE_LABELS: &[&str] = &["skip", "flag", "reply"];

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Thresholds {
    pub k: usize,
    /// Top neighbour similarity must reach this.
    pub min_top_sim: f32,
    /// Top similarity minus the best non-skip neighbour's similarity.
    pub min_margin: f32,
    /// Share of pre-filterable messages that still go to the reasoner.
    pub spot_check_pct: u8,
    /// Spot-check disagreement rate (over the last `window`) that disables
    /// the pre-filter.
    pub disagreement_bound: f32,
    pub window: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        // Conservative defaults; run `augmentagent triage-prefilter calibrate`
        // on your own store and set the env overrides from its recommendation.
        // On the reference store: k=16 removed the `flag` disagreements that
        // k=8 let through (flagged newsletters sit inside the skip cloud, so
        // unanimity over a wider neighbourhood is what catches them), and a
        // margin requirement lowered agreement rather than raising it, so the
        // margin gate is off by default.
        Self {
            k: 16,
            min_top_sim: 0.90,
            min_margin: 0.0,
            spot_check_pct: 10,
            disagreement_bound: 0.05,
            window: 50,
        }
    }
}

impl Thresholds {
    pub fn from_env() -> Self {
        let mut t = Self::default();
        let f = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
        };
        if let Some(v) = f(ENV_MIN_SIM) {
            t.min_top_sim = v;
        }
        if let Some(v) = f(ENV_MIN_MARGIN) {
            t.min_margin = v;
        }
        if let Some(v) = std::env::var(ENV_K)
            .ok()
            .and_then(|v| v.trim().parse().ok())
        {
            t.k = v;
        }
        if let Some(v) = std::env::var(ENV_SPOT_RATE)
            .ok()
            .and_then(|v| v.trim().parse().ok())
        {
            t.spot_check_pct = v;
        }
        t
    }

    pub fn version(&self) -> String {
        format!(
            "v1:k{}:sim{:.3}:margin{:.3}",
            self.k, self.min_top_sim, self.min_margin
        )
    }
}

pub fn enabled_from(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub fn ensure_tables(c: &Connection) -> augmentagent_store::rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS triage_prefilter_decisions (
             message_id         TEXT PRIMARY KEY,
             verdict            TEXT NOT NULL,
             reasoner_decision  TEXT,
             spot_check         INTEGER NOT NULL,
             top_sim            REAL NOT NULL,
             margin             REAL NOT NULL,
             neighbours         TEXT NOT NULL,
             thresholds_version TEXT NOT NULL,
             ts_ms              INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_tpd_ts ON triage_prefilter_decisions(ts_ms);
         CREATE TABLE IF NOT EXISTS triage_prefilter_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
}

/// One labelled, embedded, live-triaged message.
#[derive(Debug, Clone, PartialEq)]
pub struct Labelled {
    pub message_id: String,
    pub label: String,
    pub first_seen_ms: i64,
}

/// Labelled neighbours with their vectors, for one model.
pub struct LabelledSet {
    pub items: Vec<Labelled>,
    dim: usize,
    data: Vec<f32>,
}

impl LabelledSet {
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn vector(&self, i: usize) -> &[f32] {
        &self.data[i * self.dim..(i + 1) * self.dim]
    }

    /// Eligible rows: live-triaged, reasoner-labelled, with a vector for
    /// `id`, excluding rows decided by the pre-filter itself.
    pub fn load(c: &Connection, id: &crate::embedder::ModelId) -> anyhow::Result<Self> {
        ensure_tables(c)?;
        let platforms = ELIGIBLE_PLATFORMS
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(",");
        let labels = ELIGIBLE_LABELS
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT e.messageId, e.triageResult, e.firstSeenAt, v.vector
               FROM emails e
               JOIN message_chunks k ON k.first_message_id = e.messageId
               JOIN message_vectors v ON v.chunk_id = k.chunk_id AND v.provider = ?1 AND v.model = ?2 AND v.dim = ?3
              WHERE e.platform IN ({platforms}) AND e.triageResult IN ({labels})
                AND e.agentProcessedAt IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM triage_prefilter_decisions d
                                 WHERE d.message_id = e.messageId AND d.reasoner_decision IS NULL)
              ORDER BY e.firstSeenAt"
        );
        let mut stmt = c.prepare(&sql)?;
        let mut items = Vec::new();
        let mut data = Vec::new();
        for row in stmt.query_map(params![id.provider, id.model, id.dim as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })? {
            let (mid, label, ts, blob) = row?;
            data.extend(from_blob(&blob, id.dim)?);
            items.push(Labelled {
                message_id: mid,
                label,
                first_seen_ms: ts,
            });
        }
        Ok(Self {
            items,
            dim: id.dim,
            data,
        })
    }

    fn from_parts(items: Vec<Labelled>, dim: usize, data: Vec<f32>) -> Self {
        Self { items, dim, data }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Neighbour {
    pub message_id: String,
    pub label: String,
    pub sim: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    pub top_sim: f32,
    /// `top_sim` minus the best non-skip similarity in the whole set
    /// (1.0 + top_sim when there is no non-skip row at all).
    pub margin: f32,
    pub neighbours: Vec<Neighbour>,
    pub unanimous_skip: bool,
    pub decided: bool,
}

/// Pure decision over a labelled set. `exclude` skips one index (leave-one-
/// out for calibration).
pub fn assess(
    set: &LabelledSet,
    query: &[f32],
    t: &Thresholds,
    exclude: Option<usize>,
) -> Assessment {
    let mut scored: Vec<(f32, usize)> = (0..set.len())
        .filter(|&i| Some(i) != exclude)
        .map(|i| (dot(query, set.vector(i)), i))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let neighbours: Vec<Neighbour> = scored
        .iter()
        .take(t.k)
        .map(|(sim, i)| Neighbour {
            message_id: set.items[*i].message_id.clone(),
            label: set.items[*i].label.clone(),
            sim: *sim,
        })
        .collect();
    let top_sim = neighbours.first().map(|n| n.sim).unwrap_or(-1.0);
    let best_non_skip = scored
        .iter()
        .find(|(_, i)| set.items[*i].label != "skip")
        .map(|(s, _)| *s);
    let margin = match best_non_skip {
        Some(s) => top_sim - s,
        None => 1.0 + top_sim,
    };
    let unanimous_skip = neighbours.len() == t.k && neighbours.iter().all(|n| n.label == "skip");
    let decided = unanimous_skip && top_sim >= t.min_top_sim && margin >= t.min_margin;
    Assessment {
        top_sim,
        margin,
        neighbours,
        unanimous_skip,
        decided,
    }
}

/// Deterministic spot-check sampler: FNV-1a of the message id.
pub fn is_spot_check(message_id: &str, pct: u8) -> bool {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in message_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h % 100) < pct as u64
}

/// Store-backed pre-filter state: the labelled set (refreshable), thresholds,
/// and the audit/kill-switch logic. The embedding itself is done by the
/// caller (it owns the embedder) so this stays testable with hand vectors.
pub struct Prefilter {
    thresholds: Thresholds,
    set: RwLock<Option<LabelledSet>>,
}

impl Prefilter {
    pub fn new(thresholds: Thresholds) -> Self {
        Self {
            thresholds,
            set: RwLock::new(None),
        }
    }

    pub fn thresholds(&self) -> &Thresholds {
        &self.thresholds
    }

    pub fn refresh(&self, c: &Connection, id: &crate::embedder::ModelId) -> anyhow::Result<usize> {
        let set = LabelledSet::load(c, id)?;
        let n = set.len();
        *self
            .set
            .write()
            .map_err(|_| anyhow::anyhow!("prefilter lock poisoned"))? = Some(set);
        Ok(n)
    }

    pub fn assess(&self, query: &[f32]) -> Option<Assessment> {
        let guard = self.set.read().ok()?;
        let set = guard.as_ref()?;
        if set.is_empty() {
            return None;
        }
        Some(assess(set, query, &self.thresholds, None))
    }

    /// Env switch on and not auto-disabled.
    pub fn enabled(&self, c: &Connection) -> bool {
        enabled_from(std::env::var(ENV_ENABLED).ok().as_deref()) && !auto_disabled(c)
    }

    /// Record a decision; for spot-checks, re-evaluate the disagreement rate
    /// and disable the pre-filter when it exceeds the bound.
    pub fn record(
        &self,
        c: &Connection,
        message_id: &str,
        a: &Assessment,
        reasoner_decision: Option<&str>,
        spot_check: bool,
    ) -> anyhow::Result<()> {
        ensure_tables(c)?;
        c.execute(
            "INSERT OR REPLACE INTO triage_prefilter_decisions
                (message_id, verdict, reasoner_decision, spot_check, top_sim, margin, neighbours, thresholds_version, ts_ms)
             VALUES (?1, 'skip', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                message_id,
                reasoner_decision,
                spot_check,
                a.top_sim as f64,
                a.margin as f64,
                serde_json::to_string(&a.neighbours)?,
                self.thresholds.version(),
                now_ms(),
            ],
        )?;
        if spot_check {
            let (checked, disagreed) = recent_spot_checks(c, self.thresholds.window)?;
            if checked >= 10
                && disagreed as f32 / checked as f32 > self.thresholds.disagreement_bound
            {
                c.execute(
                    "INSERT OR REPLACE INTO triage_prefilter_meta (key, value) VALUES ('auto_disabled', ?1)",
                    [format!(
                        "{}: {disagreed}/{checked} recent spot-checks disagreed with the reasoner (bound {:.0}%)",
                        now_ms(),
                        self.thresholds.disagreement_bound * 100.0
                    )],
                )?;
            }
        }
        Ok(())
    }
}

pub fn auto_disabled(c: &Connection) -> bool {
    c.query_row(
        "SELECT value FROM triage_prefilter_meta WHERE key = 'auto_disabled'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

pub fn clear_auto_disable(c: &Connection) -> augmentagent_store::rusqlite::Result<usize> {
    ensure_tables(c)?;
    c.execute(
        "DELETE FROM triage_prefilter_meta WHERE key = 'auto_disabled'",
        [],
    )
}

/// (spot-checks in the window, of which disagreed). A spot-check disagrees
/// when the reasoner decided anything other than `skip`.
fn recent_spot_checks(
    c: &Connection,
    window: usize,
) -> augmentagent_store::rusqlite::Result<(usize, usize)> {
    let mut stmt = c.prepare(
        "SELECT reasoner_decision FROM triage_prefilter_decisions WHERE spot_check = 1 ORDER BY ts_ms DESC LIMIT ?1",
    )?;
    let rows: Vec<Option<String>> = stmt
        .query_map([window as i64], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let checked = rows.len();
    let disagreed = rows.iter().filter(|r| r.as_deref() != Some("skip")).count();
    Ok((checked, disagreed))
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Stats {
    pub decided_without_reasoner: i64,
    pub spot_checks: i64,
    pub spot_check_disagreements: i64,
    pub auto_disabled: Option<String>,
    pub thresholds: Option<Thresholds>,
}

pub fn stats(c: &Connection, t: &Thresholds) -> anyhow::Result<Stats> {
    ensure_tables(c)?;
    Ok(Stats {
        decided_without_reasoner: c.query_row(
            "SELECT COUNT(*) FROM triage_prefilter_decisions WHERE spot_check = 0",
            [],
            |r| r.get(0),
        )?,
        spot_checks: c.query_row("SELECT COUNT(*) FROM triage_prefilter_decisions WHERE spot_check = 1", [], |r| r.get(0))?,
        spot_check_disagreements: c.query_row(
            "SELECT COUNT(*) FROM triage_prefilter_decisions WHERE spot_check = 1 AND COALESCE(reasoner_decision, '') <> 'skip'",
            [],
            |r| r.get(0),
        )?,
        auto_disabled: c
            .query_row("SELECT value FROM triage_prefilter_meta WHERE key = 'auto_disabled'", [], |r| r.get(0))
            .optional()?,
        thresholds: Some(*t),
    })
}

// ---- offline calibration ---------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CalibrationPoint {
    pub min_top_sim: f32,
    pub min_margin: f32,
    /// Share of test messages the pre-filter would decide.
    pub coverage: f32,
    /// Of those, share the reasoner also skipped.
    pub agreement: f32,
    pub decided: usize,
    pub disagreed: usize,
    /// Reasoner labels of the disagreements.
    pub disagreed_labels: HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Calibration {
    pub train: usize,
    pub test: usize,
    pub k: usize,
    pub grid: Vec<CalibrationPoint>,
    /// Highest-coverage point whose disagreement is at or under `max_disagreement`.
    pub recommended: Option<CalibrationPoint>,
    pub max_disagreement: f32,
}

/// Time-ordered split: the oldest `train_share` of labelled rows are the
/// neighbour set, the rest are queries (leave-one-out is unnecessary since
/// test rows are not in the train set). Works from stored vectors only.
pub fn calibrate(
    set: &LabelledSet,
    k: usize,
    train_share: f32,
    max_disagreement: f32,
) -> Calibration {
    let n = set.len();
    let split = ((n as f32) * train_share).round() as usize;
    let train_items: Vec<Labelled> = set.items[..split].to_vec();
    let train_data: Vec<f32> = set.data[..split * set.dim].to_vec();
    let train = LabelledSet::from_parts(train_items, set.dim, train_data);
    let test: Vec<usize> = (split..n).collect();
    let sims = [0.80f32, 0.85, 0.88, 0.90, 0.92, 0.94, 0.96, 0.98];
    let margins = [0.0f32, 0.02, 0.05, 0.10];
    // Assess each test row once with the loosest thresholds; re-apply
    // thresholds per grid point from the stored numbers.
    let loose = Thresholds {
        k,
        min_top_sim: -1.0,
        min_margin: -2.0,
        ..Default::default()
    };
    let assessed: Vec<(Assessment, &str)> = test
        .iter()
        .map(|&i| {
            (
                assess(&train, set.vector(i), &loose, None),
                set.items[i].label.as_str(),
            )
        })
        .collect();
    let mut grid = Vec::new();
    for &s in &sims {
        for &m in &margins {
            let mut decided = 0usize;
            let mut disagreed = 0usize;
            let mut labels: HashMap<String, usize> = HashMap::new();
            for (a, label) in &assessed {
                if a.unanimous_skip && a.top_sim >= s && a.margin >= m {
                    decided += 1;
                    if *label != "skip" {
                        disagreed += 1;
                        *labels.entry(label.to_string()).or_default() += 1;
                    }
                }
            }
            grid.push(CalibrationPoint {
                min_top_sim: s,
                min_margin: m,
                coverage: if test.is_empty() {
                    0.0
                } else {
                    decided as f32 / test.len() as f32
                },
                agreement: if decided == 0 {
                    1.0
                } else {
                    1.0 - disagreed as f32 / decided as f32
                },
                decided,
                disagreed,
                disagreed_labels: labels,
            });
        }
    }
    let recommended = grid
        .iter()
        .filter(|p| p.decided > 0 && (1.0 - p.agreement) <= max_disagreement)
        .max_by(|a, b| {
            a.coverage
                .partial_cmp(&b.coverage)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();
    Calibration {
        train: split,
        test: test.len(),
        k,
        grid,
        recommended,
        max_disagreement,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::l2_normalize;

    fn unit(v: &[f32]) -> Vec<f32> {
        let mut x = v.to_vec();
        l2_normalize(&mut x);
        x
    }

    /// Two clusters: newsletters near (1,0,0), questions near (0,1,0).
    fn set(k_skips: usize, replies: usize) -> LabelledSet {
        let mut items = Vec::new();
        let mut data = Vec::new();
        for i in 0..k_skips {
            items.push(Labelled {
                message_id: format!("s{i}"),
                label: "skip".into(),
                first_seen_ms: i as i64,
            });
            data.extend(unit(&[1.0, 0.0, 0.02 * i as f32]));
        }
        for i in 0..replies {
            items.push(Labelled {
                message_id: format!("r{i}"),
                label: "reply".into(),
                first_seen_ms: 1000 + i as i64,
            });
            data.extend(unit(&[0.0, 1.0, 0.02 * i as f32]));
        }
        LabelledSet::from_parts(items, 3, data)
    }

    #[test]
    fn unanimous_close_neighbourhood_predicts() {
        let s = set(10, 5);
        let t = Thresholds {
            k: 8,
            min_top_sim: 0.9,
            min_margin: 0.05,
            ..Default::default()
        };
        let a = assess(&s, &unit(&[1.0, 0.05, 0.0]), &t, None);
        assert!(a.unanimous_skip && a.decided, "{a:?}");
        assert!(a.top_sim > 0.99);
        assert!(a.margin > 0.5);
        assert_eq!(a.neighbours.len(), 8);
    }

    #[test]
    fn split_far_and_thin_neighbourhoods_fall_through() {
        let t = Thresholds {
            k: 8,
            min_top_sim: 0.9,
            min_margin: 0.05,
            ..Default::default()
        };
        // Split: query equidistant from both clusters → neighbours mix labels.
        let a = assess(&set(10, 10), &unit(&[1.0, 1.0, 0.0]), &t, None);
        assert!(!a.unanimous_skip && !a.decided, "{a:?}");
        // Far: all-skip set but query is far from it.
        let a = assess(&set(10, 0), &unit(&[0.0, 0.0, 1.0]), &t, None);
        assert!(a.unanimous_skip && !a.decided, "{a:?}");
        // Thin: fewer eligible rows than k.
        let a = assess(&set(3, 0), &unit(&[1.0, 0.0, 0.0]), &t, None);
        assert!(!a.unanimous_skip && !a.decided);
    }

    #[test]
    fn margin_uses_the_best_non_skip_anywhere_not_just_top_k() {
        // 8 skips very close, one reply also close: margin must be small.
        let mut s = set(8, 0);
        s.items.push(Labelled {
            message_id: "r".into(),
            label: "reply".into(),
            first_seen_ms: 5,
        });
        s.data.extend(unit(&[1.0, 0.3, 0.0]));
        let t = Thresholds {
            k: 8,
            min_top_sim: 0.9,
            min_margin: 0.10,
            ..Default::default()
        };
        let a = assess(&s, &unit(&[1.0, 0.1, 0.0]), &t, None);
        assert!(a.unanimous_skip, "top-8 are still the skips");
        assert!(
            a.margin < 0.10,
            "margin {:.3} reflects the nearby reply",
            a.margin
        );
        assert!(!a.decided);
    }

    #[test]
    fn spot_check_sampler_is_deterministic_and_honours_the_rate() {
        assert_eq!(is_spot_check("abc", 10), is_spot_check("abc", 10));
        let n = 10_000;
        let hits = (0..n)
            .filter(|i| is_spot_check(&format!("msg-{i}"), 10))
            .count();
        assert!((800..1200).contains(&hits), "{hits} of {n} at 10%");
        assert_eq!(
            (0..n)
                .filter(|i| is_spot_check(&format!("m{i}"), 0))
                .count(),
            0
        );
        assert_eq!(
            (0..n)
                .filter(|i| is_spot_check(&format!("m{i}"), 100))
                .count(),
            n
        );
    }

    #[test]
    fn thresholds_env_and_version() {
        let t = Thresholds::default();
        assert_eq!(t.version(), "v1:k16:sim0.900:margin0.000");
        assert!(!enabled_from(None) && !enabled_from(Some("0")) && enabled_from(Some("1")));
    }

    #[test]
    fn calibration_splits_by_time_and_recommends_the_widest_safe_point() {
        // 40 skips then 10 replies (newer). Test = newest 20%: 10 replies →
        // a pre-filter deciding any of them disagrees; safe points decide none.
        let s = set(40, 10);
        let c = calibrate(&s, 8, 0.8, 0.01);
        assert_eq!((c.train, c.test), (40, 10));
        assert!(
            c.grid.iter().all(|p| p.disagreed == p.decided),
            "every decided test row is a reply here"
        );
        assert!(
            c.recommended.is_none(),
            "nothing safe to recommend when all test rows disagree"
        );
        // Newer skips near the cluster: a safe, covering point exists.
        let s = set(50, 0);
        let c = calibrate(&s, 8, 0.8, 0.01);
        let r = c.recommended.expect("recommendation");
        assert!(r.coverage > 0.9 && r.agreement == 1.0, "{r:?}");
    }

    // ---- store-backed ----

    use augmentagent_store::Store;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("t.db")).unwrap();
        (d, s)
    }

    fn seed_email(
        s: &Store,
        id: &str,
        platform: &str,
        triage: Option<&str>,
        processed: bool,
        vec: Option<&[f32]>,
    ) {
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, firstSeenAt, triageResult, agentProcessedAt, platform, kind)
                 VALUES (?1, ?1, 'x@example.com', 's', 'b', '2026-01-01T00:00:00Z', 1, ?2, ?3, ?4, 'dm')",
                params![id, triage, if processed { Some(1i64) } else { None }, platform],
            )?;
            crate::vectors::ensure_tables(c)?;
            c.execute(
                "INSERT INTO message_chunks (chunk_id, conversation_id, first_message_id, last_message_id, message_count, start_ts_ms, end_ts_ms, text_hash, sealed, params_version)
                 VALUES (?1, ?2, ?2, ?2, 1, 1, 1, 'h', 1, 'v')",
                params![format!("{id}\u{1f}{id}"), id],
            )?;
            if let Some(v) = vec {
                c.execute(
                    "INSERT INTO message_vectors (chunk_id, provider, model, dim, vector, text_hash, updated_at_ms) VALUES (?1, 'stub', 'hash', 3, ?2, 'h', 1)",
                    params![format!("{id}\u{1f}{id}"), crate::vectors::to_blob(v)],
                )?;
            }
            Ok(())
        })
        .unwrap();
    }

    fn id() -> crate::embedder::ModelId {
        crate::embedder::ModelId {
            provider: "stub".into(),
            model: "hash".into(),
            dim: 3,
        }
    }

    #[test]
    fn eligible_corpus_excludes_history_imports_untriaged_and_prefilter_decided_rows() {
        let (_d, s) = store();
        let v = unit(&[1.0, 0.0, 0.0]);
        seed_email(&s, "live-skip", "gmail", Some("skip"), true, Some(&v));
        seed_email(&s, "live-reply", "gmail", Some("reply"), true, Some(&v));
        seed_email(
            &s,
            "history",
            "discord",
            Some("digest_only"),
            true,
            Some(&v),
        );
        seed_email(
            &s,
            "imported-chat",
            "whatsapp",
            Some("digest_only"),
            true,
            Some(&v),
        );
        seed_email(&s, "untriaged", "gmail", None, false, Some(&v));
        seed_email(&s, "no-vector", "gmail", Some("skip"), true, None);
        seed_email(&s, "self-decided", "gmail", Some("skip"), true, Some(&v));
        let pf = Prefilter::new(Thresholds::default());
        s.with_conn(|c| {
            let a = Assessment {
                top_sim: 0.99,
                margin: 0.5,
                neighbours: vec![],
                unanimous_skip: true,
                decided: true,
            };
            pf.record(c, "self-decided", &a, None, false).unwrap();
            Ok(())
        })
        .unwrap();
        let set = s
            .with_conn(|c| Ok(LabelledSet::load(c, &id()).unwrap()))
            .unwrap();
        let mut ids: Vec<&str> = set.items.iter().map(|i| i.message_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["live-reply", "live-skip"]);
    }

    #[test]
    fn spot_check_disagreements_over_the_bound_auto_disable_and_stats_report_it() {
        let (_d, s) = store();
        let pf = Prefilter::new(Thresholds {
            window: 20,
            disagreement_bound: 0.05,
            ..Default::default()
        });
        std::env::set_var(ENV_ENABLED, "1");
        s.with_conn(|c| {
            assert!(pf.enabled(c));
            let a = Assessment {
                top_sim: 0.95,
                margin: 0.2,
                neighbours: vec![],
                unanimous_skip: true,
                decided: true,
            };
            for i in 0..9 {
                pf.record(c, &format!("ok{i}"), &a, Some("skip"), true)
                    .unwrap();
            }
            assert!(
                pf.enabled(c),
                "9 agreeing spot-checks, below the 10 minimum"
            );
            pf.record(c, "bad1", &a, Some("reply"), true).unwrap();
            assert!(!pf.enabled(c), "1/10 = 10% disagreement > 5% bound");
            let st = stats(c, pf.thresholds()).unwrap();
            assert_eq!((st.spot_checks, st.spot_check_disagreements), (10, 1));
            assert!(st.auto_disabled.is_some());
            clear_auto_disable(c)?;
            assert!(pf.enabled(c));
            pf.record(c, "solo", &a, None, false).unwrap();
            assert_eq!(
                stats(c, pf.thresholds()).unwrap().decided_without_reasoner,
                1
            );
            Ok(())
        })
        .unwrap();
        std::env::remove_var(ENV_ENABLED);
    }

    #[test]
    fn decision_records_neighbours_similarity_and_threshold_version() {
        let (_d, s) = store();
        let pf = Prefilter::new(Thresholds::default());
        s.with_conn(|c| {
            let a = Assessment {
                top_sim: 0.97,
                margin: 0.3,
                neighbours: vec![Neighbour { message_id: "n1".into(), label: "skip".into(), sim: 0.97 }],
                unanimous_skip: true,
                decided: true,
            };
            pf.record(c, "m", &a, None, false).unwrap();
            let (nb, ver, sim): (String, String, f64) = c.query_row(
                "SELECT neighbours, thresholds_version, top_sim FROM triage_prefilter_decisions WHERE message_id='m'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert!(nb.contains("\"n1\""));
            assert_eq!(ver, Thresholds::default().version());
            assert!((sim - 0.97).abs() < 1e-6);
            Ok(())
        })
        .unwrap();
    }
}
