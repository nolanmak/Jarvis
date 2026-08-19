//! Computed page freshness (#642 phase 2).
//!
//! "Is this still true?" is answered by *computation, not judgment*: a
//! page's fact age is the newest `emails.firstSeenAt` across every message
//! it cites — the `sources:` frontmatter list plus the inline free-text
//! `m: <messageId>` per-claim cites that ingest has always written and
//! nothing ever parsed. `updated:` is deliberately ignored: it is
//! model-maintained prose, not evidence.
//!
//! We do NOT hash source content (local-deepwiki's `sourcesHash`): our
//! sources are Gmail messageIds into an append-only table, so a content
//! hash is a constant and would show permanent green over exactly the
//! facts most likely to be wrong.
//!
//! Rule G1 (adopted from local-deepwiki's actual empty-set bug): **no
//! resolvable evidence must never render as fresh.** A page whose cited
//! ids all fail to resolve — or that cites nothing — reports
//! `as_of = None` (unknown), never "current".
//!
//! Vocabulary is borrowed from Google's OKF v0.2 — field *names* only, no
//! spec conformance, and `sources:` is never reshaped into OKF's
//! mappings-with-resource form (that shape would empty the citation
//! allowlist in `migrate::validate_citations`). All OKF fields are
//! read-side: the owner (or a later tool) may hand-mark pages with
//! `status:` / `stale_after:` / `verified:`, and this module honors them:
//!
//! - `status: draft | stable | deprecated` — absent means stable.
//! - `stale_after: YYYY-MM-DD` — an explicit expiry; lint flags pages past it.
//! - `verified: [{ by, at }]` — a manual "still true as of `at`" attestation;
//!   the newest `at` counts as a freshness event alongside cited evidence.

use std::collections::BTreeSet;

use serde_yaml_ng::Value;
use time::{Month, OffsetDateTime};
// Re-exported so callers (the CLI report) can name dates without their own
// `time` dependency.
pub use time::Date;

use crate::migrate::{parse_sources, split_frontmatter};

/// OKF `status:`. Absent = `Stable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageStatus {
    Draft,
    Stable,
    Deprecated,
}

/// Everything freshness-relevant we can compute for one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    pub status: PageStatus,
    /// Newest freshness event: max over resolved cited ids' `firstSeenAt`
    /// and `verified[].at`. `None` = unknown (G1) — never render as fresh.
    pub as_of: Option<Date>,
    /// Distinct message ids the page cites (`sources:` + inline `m:`).
    pub cited: usize,
    /// How many of those resolved to a known email.
    pub resolved: usize,
    /// OKF explicit expiry, if the page carries one.
    pub stale_after: Option<Date>,
}

impl Freshness {
    /// Age in whole days relative to `today`, if evidence exists.
    pub fn age_days(&self, today: Date) -> Option<i64> {
        self.as_of.map(|d| (today - d).whole_days())
    }

    /// Past its explicit OKF expiry?
    pub fn past_stale_after(&self, today: Date) -> bool {
        self.stale_after.map(|d| today > d).unwrap_or(false)
    }
}

/// Compute freshness for a page. `resolve` maps a cited message id to its
/// `emails.firstSeenAt` (epoch **milliseconds**), `None` when unknown.
pub fn compute(page: &str, resolve: &dyn Fn(&str) -> Option<i64>) -> Freshness {
    let fm = split_frontmatter(page).map(|(inner, _)| inner).unwrap_or("");
    let ids = cited_ids(page);

    let mut resolved = 0usize;
    let mut latest_ms: Option<i64> = None;
    for id in &ids {
        if let Some(ms) = resolve(id) {
            resolved += 1;
            latest_ms = Some(latest_ms.map_or(ms, |cur| cur.max(ms)));
        }
    }

    let mut as_of = latest_ms.and_then(ms_to_date);
    if let Some(v) = latest_verified_at(fm) {
        as_of = Some(as_of.map_or(v, |cur| cur.max(v)));
    }

    Freshness {
        status: parse_status(fm),
        as_of,
        cited: ids.len(),
        resolved,
        stale_after: frontmatter_date(fm, "stale_after"),
    }
}

/// Every message id a page cites: the `sources:` frontmatter list plus the
/// inline `m: <id>` free-text cites in the body.
pub fn cited_ids(page: &str) -> BTreeSet<String> {
    let (fm, body) = match split_frontmatter(page) {
        Some((inner, close_off)) => {
            let body = match page[close_off..].find('\n') {
                Some(nl) => &page[close_off + nl + 1..],
                None => "",
            };
            (inner, body)
        }
        None => ("", page),
    };
    let mut ids = parse_sources(fm);
    collect_inline_cites(body, &mut ids);
    ids
}

/// Scan body text for `m: <id>` cites — `(m: 19dac...)` in prose and
/// `| m: 19dac...` in timeline rows. The char before `m` must not be
/// alphanumeric so `team: alpha` / `7pm: standup` don't false-positive;
/// the id charset is conservative and the resolver gates anyway.
fn collect_inline_cites(body: &str, ids: &mut BTreeSet<String>) {
    let bytes = body.as_bytes();
    for (pos, _) in body.match_indices("m:") {
        if pos > 0 && (bytes[pos - 1] as char).is_ascii_alphanumeric() {
            continue;
        }
        let rest = body[pos + 2..].trim_start_matches([' ', '\t']);
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .collect();
        if token.len() >= 6 {
            ids.insert(token);
        }
    }
}

fn parse_status(fm: &str) -> PageStatus {
    match frontmatter_str(fm, "status").as_deref() {
        Some("draft") => PageStatus::Draft,
        Some("deprecated") => PageStatus::Deprecated,
        _ => PageStatus::Stable,
    }
}

/// Newest `verified[].at` date, if any parse.
fn latest_verified_at(fm: &str) -> Option<Date> {
    let value: Value = serde_yaml_ng::from_str(fm).ok()?;
    let seq = value.as_mapping()?.get(Value::String("verified".into()))?.as_sequence()?;
    seq.iter()
        .filter_map(|entry| {
            let at = entry.as_mapping()?.get(Value::String("at".into()))?;
            parse_iso_date(&yaml_scalar_to_string(at)?)
        })
        .max()
}

fn frontmatter_str(fm: &str, key: &str) -> Option<String> {
    let value: Value = serde_yaml_ng::from_str(fm).ok()?;
    let v = value.as_mapping()?.get(Value::String(key.into()))?;
    yaml_scalar_to_string(v)
}

fn frontmatter_date(fm: &str, key: &str) -> Option<Date> {
    parse_iso_date(&frontmatter_str(fm, key)?)
}

/// YAML may parse an unquoted `2026-08-19` as a string; keep scalars only.
fn yaml_scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse the leading `YYYY-MM-DD` of an ISO date or datetime. Manual so we
/// accept both `2026-08-19` and `2026-08-19T10:00:00Z` without caring about
/// `time`'s parser feature matrix.
pub fn parse_iso_date(s: &str) -> Option<Date> {
    let s = s.trim();
    let date_part = s.get(..10)?;
    let mut it = date_part.split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u8 = it.next()?.parse().ok()?;
    let d: u8 = it.next()?.parse().ok()?;
    Date::from_calendar_date(y, Month::try_from(m).ok()?, d).ok()
}

/// Today in UTC — the reference point for age buckets. Lives here so
/// callers don't need their own `time` dependency.
pub fn today_utc() -> Date {
    OffsetDateTime::now_utc().date()
}

/// Epoch milliseconds (the `emails.firstSeenAt` unit) → UTC date.
pub fn ms_to_date(ms: i64) -> Option<Date> {
    OffsetDateTime::from_unix_timestamp(ms / 1000)
        .ok()
        .map(|t| t.date())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u8, day: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), day).unwrap()
    }

    // 2026-04-18 21:55:30 UTC — the issue's verified join example.
    const APR_18_MS: i64 = 1776549330000;

    #[test]
    fn fact_age_is_max_first_seen_across_sources() {
        let page = "---\nkind: person\nsources: [old1111, new2222]\n---\n\n## Identity\nx\n";
        let f = compute(page, &|id| match id {
            "old1111" => Some(APR_18_MS - 86_400_000 * 10),
            "new2222" => Some(APR_18_MS),
            _ => None,
        });
        assert_eq!(f.as_of, Some(d(2026, 4, 18)));
        assert_eq!((f.cited, f.resolved), (2, 2));
    }

    #[test]
    fn g1_no_sources_is_unknown_never_fresh() {
        let page = "---\nkind: person\nsources: []\n---\n\nbody\n";
        let f = compute(page, &|_| Some(APR_18_MS)); // resolver would say fresh — must not be consulted into a date
        assert_eq!(f.as_of, None, "empty evidence set must be unknown, not fresh");
        assert_eq!(f.cited, 0);
    }

    #[test]
    fn g1_unresolvable_sources_are_unknown_never_fresh() {
        let page = "---\nkind: person\nsources: [ghost001]\n---\n\nbody\n";
        let f = compute(page, &|_| None);
        assert_eq!(f.as_of, None);
        assert_eq!((f.cited, f.resolved), (1, 0));
    }

    #[test]
    fn inline_m_cites_are_parsed_and_counted() {
        let page = "---\nkind: thread\nsources: [aaa111]\n---\n\n## Timeline\n- **2026-04-20** | bot | closed | m: bbb222\n- note (m: ccc333)\n";
        let ids = cited_ids(page);
        assert_eq!(
            ids.iter().collect::<Vec<_>>(),
            ["aaa111", "bbb222", "ccc333"].iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn inline_scan_rejects_word_suffix_and_short_tokens() {
        let body = "the team: alpha meets at 7pm: standup (m: abc)\n";
        let mut ids = BTreeSet::new();
        collect_inline_cites(body, &mut ids);
        assert!(ids.is_empty(), "got {ids:?}");
    }

    #[test]
    fn verified_at_counts_as_freshness_event() {
        let page = "---\nkind: person\nsources: [aaa111]\nverified:\n  - by: human:nolan\n    at: 2026-08-01\n---\n\nx\n";
        let f = compute(page, &|_| Some(APR_18_MS));
        assert_eq!(f.as_of, Some(d(2026, 8, 1)), "newest of evidence vs verified wins");
    }

    #[test]
    fn verified_alone_beats_unknown_but_only_when_present() {
        let page = "---\nkind: person\nsources: [ghost001]\nverified:\n  - { by: human:nolan, at: 2026-07-15T09:00:00Z }\n---\n\nx\n";
        let f = compute(page, &|_| None);
        assert_eq!(f.as_of, Some(d(2026, 7, 15)));
    }

    #[test]
    fn okf_status_and_stale_after_are_read() {
        let page = "---\nkind: project\nstatus: deprecated\nstale_after: 2026-01-31\nsources: [aaa111]\n---\n\nx\n";
        let f = compute(page, &|_| Some(APR_18_MS));
        assert_eq!(f.status, PageStatus::Deprecated);
        assert!(f.past_stale_after(d(2026, 2, 1)));
        assert!(!f.past_stale_after(d(2026, 1, 31)));
    }

    #[test]
    fn status_absent_means_stable() {
        let f = compute("---\nkind: person\nsources: []\n---\n", &|_| None);
        assert_eq!(f.status, PageStatus::Stable);
    }

    #[test]
    fn iso_datetime_and_date_both_parse() {
        assert_eq!(parse_iso_date("2026-08-19"), Some(d(2026, 8, 19)));
        assert_eq!(parse_iso_date("2026-08-19T10:00:00Z"), Some(d(2026, 8, 19)));
        assert_eq!(parse_iso_date("not-a-date"), None);
    }
}
