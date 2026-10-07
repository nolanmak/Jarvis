//! `augmentagent db compact` and the daemon's hourly compaction pass (#1412).
//!
//! `actions.originalBody` repeated `emails.body` for every row that never
//! produces a reply, which is most of them, and was about 40% of the
//! database file. [`Store::compact_action_bodies`] drops only an exact
//! duplicate; this module decides when, and in what size steps.
//!
//! Dropping the text frees pages inside the file; the file itself shrinks
//! only on a `VACUUM`, which rewrites the whole database and is therefore
//! never automatic (`db compact --vacuum`).

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use augmentagent_store::{BodyCompaction, Store};

pub const DEFAULT_RETENTION_DAYS: f64 = 30.0;
/// Rows per transaction: small enough that the daemon's writes never wait.
pub const BATCH_ROWS: usize = 200;
/// Batches per hourly pass; the backlog drains over a few passes.
const BATCHES_PER_PASS: usize = 50;
const PAUSE_BETWEEN_BATCHES: Duration = Duration::from_millis(100);
const FIRST_PASS_DELAY: Duration = Duration::from_secs(300);
const PASS_INTERVAL: Duration = Duration::from_secs(3600);
const DAY_MS: f64 = 86_400_000.0;

/// `AUGMENTAGENT_ACTION_BODY_RETENTION_DAYS`: how long a terminal action
/// keeps its own copy of the body. `0` (or less) turns compaction off.
pub fn retention_days_from(value: Option<&str>) -> Option<f64> {
    let days = match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => DEFAULT_RETENTION_DAYS,
        Some(v) => v.parse::<f64>().ok().filter(|d| d.is_finite())?,
    };
    (days > 0.0).then_some(days)
}

pub fn retention_days() -> Option<f64> {
    retention_days_from(std::env::var("AUGMENTAGENT_ACTION_BODY_RETENTION_DAYS").ok().as_deref())
}

pub fn cutoff_ms(now_ms: i64, days: f64) -> i64 {
    now_ms - (days * DAY_MS) as i64
}

/// Compact in batches until nothing is left or `max_batches` is reached.
pub fn compact(
    store: &Store,
    cutoff: i64,
    batch: usize,
    max_batches: usize,
    pause: Duration,
) -> Result<BodyCompaction> {
    let mut total = BodyCompaction::default();
    for _ in 0..max_batches {
        let done = store.compact_action_bodies(cutoff, batch)?;
        total.rows += done.rows;
        total.bytes += done.bytes;
        if (done.rows as usize) < batch {
            break;
        }
        std::thread::sleep(pause);
    }
    Ok(total)
}

/// The daemon's pass: shortly after start, then hourly. Failures only log.
pub async fn run_loop(
    store: Arc<Store>,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let Some(days) = retention_days() else {
        tracing::info!("action-body compaction disabled (AUGMENTAGENT_ACTION_BODY_RETENTION_DAYS=0)");
        return Ok(());
    };
    let mut wait = FIRST_PASS_DELAY;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = tokio::time::sleep(wait) => {}
        }
        wait = PASS_INTERVAL;
        let store = Arc::clone(&store);
        let cutoff = cutoff_ms(chrono::Utc::now().timestamp_millis(), days);
        let pass = tokio::task::spawn_blocking(move || {
            compact(&store, cutoff, BATCH_ROWS, BATCHES_PER_PASS, PAUSE_BETWEEN_BATCHES)
        })
        .await;
        match pass {
            Ok(Ok(done)) if done.rows > 0 => tracing::info!(
                rows = done.rows,
                mb = done.bytes / (1024 * 1024),
                "compacted duplicated action bodies"
            ),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("action-body compaction failed: {e:#}"),
            Err(e) => tracing::warn!("action-body compaction task failed: {e}"),
        }
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Pages on the free list, and the page size.
fn free_pages(store: &Store) -> Result<(u64, u64, u64)> {
    Ok(store.with_conn(|c| {
        let get = |pragma: &str| c.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get::<_, i64>(0));
        Ok((get("freelist_count")? as u64, get("page_count")? as u64, get("page_size")? as u64))
    })?)
}

pub fn run_cli(
    store: &Store,
    db_path: &std::path::Path,
    dry_run: bool,
    vacuum: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let Some(days) = retention_days() else {
        bail!("compaction is turned off (AUGMENTAGENT_ACTION_BODY_RETENTION_DAYS=0)");
    };
    let cutoff = cutoff_ms(chrono::Utc::now().timestamp_millis(), days);
    let pending = store.action_body_compaction_candidates(cutoff)?;
    if dry_run {
        writeln!(
            out,
            "would drop {} duplicated bodies ({:.0} MB) from terminal actions older than {days:.0} days",
            pending.rows,
            mb(pending.bytes)
        )?;
    } else {
        let done = compact(store, cutoff, BATCH_ROWS, usize::MAX, Duration::from_millis(20))?;
        writeln!(out, "dropped {} duplicated bodies ({:.0} MB)", done.rows, mb(done.bytes))?;
    }
    let (free, pages, page_size) = free_pages(store)?;
    writeln!(
        out,
        "database: {:.0} MB, of which {:.0} MB is free pages",
        mb(pages * page_size),
        mb(free * page_size)
    )?;
    if !vacuum {
        if free * 10 > pages {
            writeln!(out, "the file shrinks only on a VACUUM: re-run with --vacuum when the daemon is quiet")?;
        }
        return Ok(());
    }
    if dry_run {
        writeln!(out, "would VACUUM (rewrites the whole file)")?;
        return Ok(());
    }
    // VACUUM writes a full copy beside the database before swapping it in.
    let need = (pages - free) * page_size;
    if let Some(avail) = crate::disk::available_bytes(db_path) {
        if avail < need + need / 10 {
            bail!(
                "not enough room to VACUUM: {:.0} MB free, {:.0} MB needed",
                mb(avail),
                mb(need + need / 10)
            );
        }
    }
    store.with_conn(|c| c.execute_batch("VACUUM"))?;
    let (_, pages_after, _) = free_pages(store)?;
    writeln!(
        out,
        "vacuumed: {:.0} MB -> {:.0} MB",
        mb(pages * page_size),
        mb(pages_after * page_size)
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_defaults_to_thirty_days_and_zero_turns_it_off() {
        assert_eq!(retention_days_from(None), Some(30.0));
        assert_eq!(retention_days_from(Some("")), Some(30.0));
        assert_eq!(retention_days_from(Some("7")), Some(7.0));
        assert_eq!(retention_days_from(Some("0")), None);
        assert_eq!(retention_days_from(Some("-3")), None);
        assert_eq!(retention_days_from(Some("soon")), None, "unparseable never guesses a cutoff");
    }

    #[test]
    fn the_cutoff_is_that_many_days_before_now() {
        assert_eq!(cutoff_ms(100 * 86_400_000, 30.0), 70 * 86_400_000);
    }

    fn seeded(rows: usize) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        store
            .with_conn(|c| {
                for i in 0..rows {
                    let body = format!("message {i} {}", "y".repeat(4000));
                    c.execute(
                        "INSERT INTO emails (messageId, fromEmail, subject, body, receivedAt, firstSeenAt) \
                         VALUES (?1, 'a@example.test', 's', ?2, 0, 0)",
                        rusqlite::params![format!("m{i}"), body],
                    )?;
                    c.execute(
                        "INSERT INTO actions (id, messageId, fromEmail, subject, originalBody, status, createdAt, updatedAt) \
                         VALUES (?1, ?2, 'a@example.test', 's', ?3, 'skipped', 0, 0)",
                        rusqlite::params![format!("a{i}"), format!("m{i}"), body],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        (dir, store)
    }

    #[test]
    fn compact_drains_in_batches_and_stops_at_the_batch_cap() {
        let (_d, store) = seeded(25);
        let capped = compact(&store, 1, 10, 2, Duration::ZERO).unwrap();
        assert_eq!(capped.rows, 20, "two batches of ten");
        let rest = compact(&store, 1, 10, 100, Duration::ZERO).unwrap();
        assert_eq!(rest.rows, 5);
        assert_eq!(compact(&store, 1, 10, 100, Duration::ZERO).unwrap().rows, 0);
    }

    #[test]
    fn the_cli_reports_a_dry_run_then_compacts_and_vacuum_shrinks_the_file() {
        let (dir, store) = seeded(300);
        let db = dir.path().join("t.db");
        let mut dry = Vec::new();
        run_cli(&store, &db, true, false, &mut dry).unwrap();
        let dry = String::from_utf8(dry).unwrap();
        assert!(dry.starts_with("would drop 300 duplicated bodies"), "{dry}");
        assert_eq!(store.action_body_compaction_candidates(i64::MAX).unwrap().rows, 300, "dry run drops nothing");

        let (_, before, _) = free_pages(&store).unwrap();
        let mut real = Vec::new();
        run_cli(&store, &db, false, true, &mut real).unwrap();
        let real = String::from_utf8(real).unwrap();
        assert!(real.contains("dropped 300 duplicated bodies"), "{real}");
        assert!(real.contains("vacuumed:"), "{real}");
        let (free, after, _) = free_pages(&store).unwrap();
        assert!(after < before, "the file is smaller: {before} -> {after} pages");
        assert_eq!(free, 0);
        // Every row and its body are still readable.
        let row = store.get_action_with_email("a7").unwrap().unwrap();
        assert!(row.action.original_body.unwrap().starts_with("message 7 "));
    }
}
