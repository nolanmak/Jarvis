//! Per-platform archive freshness, shared by `search_messages` and
//! `conversation_stats` so the two paths cannot disagree (#1429).
//!
//! A bundle-backed platform (iMessage, WhatsApp) stops advancing the moment
//! its exporter stops running — the user quit the app. That is a normal,
//! permanent condition, not ingest lag, so a query whose window starts after
//! the archive ends has to be told "the archive ends here" rather than
//! handed an empty result set.

use std::collections::BTreeMap;

use augmentagent_store::rusqlite::Connection;

/// Platforms that have a bundle reader, and so a freshness cursor to report.
/// Listing them here is what lets a query answer for a platform whose poller
/// has never run: the field is present and null rather than missing.
const BUNDLE_PLATFORMS: [&str; 2] = ["imessage", "whatsapp"];

fn in_scope(platform: &str, scope: &[String]) -> bool {
    scope.is_empty() || scope.iter().any(|p| p.eq_ignore_ascii_case(platform))
}

/// Newest message timestamp each bundle reader has seen, keyed by platform,
/// restricted to `scope` when that is non-empty. Every bundle-backed platform
/// in scope is present — `None` before its first poll — so a caller can tell
/// "no cursor yet" from "this platform has no archive" (absent).
pub fn newest_entries(
    c: &Connection,
    scope: &[String],
) -> augmentagent_store::rusqlite::Result<BTreeMap<String, Option<String>>> {
    let mut out: BTreeMap<String, Option<String>> = BUNDLE_PLATFORMS
        .iter()
        .filter(|p| in_scope(p, scope))
        .map(|p| ((*p).to_string(), None))
        .collect();
    let mut stmt = c.prepare("SELECT platform, newest_entry FROM platform_archive_state")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (platform, newest) = row?;
        if in_scope(&platform, scope) {
            out.insert(platform, newest);
        }
    }
    Ok(out)
}

/// Platforms whose archive ends strictly before the query's lower bound.
/// An unbounded query (`lower_bound` of `None`) is never stale: a freshness
/// signal that fires on every query is useless.
pub fn stale_platforms(
    cursors: &BTreeMap<String, Option<String>>,
    lower_bound_ms: Option<i64>,
) -> Vec<String> {
    let Some(bound) = lower_bound_ms else {
        return Vec::new();
    };
    cursors
        .iter()
        .filter(|(_, newest)| {
            newest
                .as_deref()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .is_some_and(|ts| ts.timestamp_millis() < bound)
        })
        .map(|(platform, _)| platform.clone())
        .collect()
}
