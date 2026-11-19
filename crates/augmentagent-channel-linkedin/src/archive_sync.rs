//! Bounded, resumable archival reads. Intentionally no triage/send dependency.
use crate::{
    archive::{ArchiveError, ArchiveReader},
    archive_store::{self as db, Coverage},
    backfill_pace::{self, Admit, Pacer},
    session_guard,
};
use augmentagent_store::Store;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub pages: usize,
    pub stopped: String,
    pub account_deferred: bool,
    /// Set with `stopped: "request_budget"`: when backfill may read again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_at_ms: Option<i64>,
    pub coverage: Coverage,
}
fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
/// When a failed read may be tried again. Throttling honours Retry-After. A
/// rejected session needs a human, so each further rejection doubles the
/// pause up to a cap instead of replaying the dead cookie every minute.
/// Call once per failed request: a session failure is counted here.
fn retry_at(store: &Store, account: &str, error: &ArchiveError, now: i64) -> anyhow::Result<i64> {
    if session_guard::is_session_failure(error) {
        return session_guard::record_failure(store, account, &error.to_string(), now);
    }
    let seconds = match error {
        ArchiveError::RateLimited(s) => *s,
        _ => 60,
    };
    Ok(now.saturating_add(seconds.saturating_mul(1000).min(i64::MAX as u64) as i64))
}

/// Why the loop must stop before sending the next request.
struct Stop {
    reason: &'static str,
    resume_at_ms: Option<i64>,
    account_deferred: bool,
}

/// Gate one outgoing request and record it. Backfill waits for its delay and
/// budget; live reads (no pacer) are recorded and never held.
async fn admit(
    pacer: Option<&Pacer>,
    store: &Store,
    account: &str,
    cancel: &CancellationToken,
    now: &(dyn Fn() -> i64 + Sync),
) -> anyhow::Result<Option<Stop>> {
    if let Some(pacer) = pacer {
        let stop = |reason, resume_at_ms, account_deferred| {
            Ok(Some(Stop {
                reason,
                resume_at_ms,
                account_deferred,
            }))
        };
        match pacer.admit(store, account, cancel).await? {
            Admit::Go => {}
            Admit::Budget { resume_at_ms } => return stop("request_budget", Some(resume_at_ms), false),
            Admit::Cancelled => return stop("cancelled", None, false),
        }
        // Another importer may have paused the account during the delay.
        if db::account_retry_at(store, account)? > now() {
            return stop("retry_deferred", None, true);
        }
    }
    backfill_pace::record_request(store, account, now())?;
    Ok(None)
}

pub async fn sync(
    reader: &ArchiveReader,
    store: &Store,
    category: &str,
    max_pages: usize,
    cancel: &CancellationToken,
) -> anyhow::Result<SyncReport> {
    sync_with_clock(reader, store, category, max_pages, cancel, &now_ms).await
}

pub async fn sync_paced(
    reader: &ArchiveReader,
    store: &Store,
    category: &str,
    max_pages: usize,
    cancel: &CancellationToken,
    pacer: &crate::backfill_pace::Pacer,
) -> anyhow::Result<SyncReport> {
    sync_inner(reader, store, category, max_pages, cancel, &|| pacer.now(), Some(pacer)).await
}

/// Clock injection for deterministic retry/recovery tests; production uses UTC.
#[doc(hidden)]
pub async fn sync_with_clock(
    reader: &ArchiveReader,
    store: &Store,
    category: &str,
    max_pages: usize,
    cancel: &CancellationToken,
    now: &(dyn Fn() -> i64 + Sync),
) -> anyhow::Result<SyncReport> {
    sync_inner(reader, store, category, max_pages, cancel, now, None).await
}

async fn sync_inner(
    reader: &ArchiveReader,
    store: &Store,
    category: &str,
    max_pages: usize,
    cancel: &CancellationToken,
    now: &(dyn Fn() -> i64 + Sync),
    pacer: Option<&Pacer>,
) -> anyhow::Result<SyncReport> {
    let account = reader.mailbox();
    let mut pages = 0;
    let mut account_deferred = false;
    let mut resume_at_ms = None;
    let mut stopped = "page_budget".to_string();
    while pages < max_pages {
        if cancel.is_cancelled() {
            stopped = "cancelled".into();
            break;
        }
        let mut request_started = now();
        if db::account_retry_at(store, account)? > request_started {
            stopped = "retry_deferred".into();
            account_deferred = true;
            break;
        }
        let cp = db::checkpoint(store, account, category)?;
        if !cp.complete {
            if cp.retry_at_ms > now() {
                stopped = "retry_deferred".into();
                break;
            }
            if let Some(stop) = admit(pacer, store, account, cancel, now).await? {
                stopped = stop.reason.into();
                resume_at_ms = stop.resume_at_ms;
                account_deferred |= stop.account_deferred;
                break;
            }
            request_started = now();
            pages += 1;
            let fetched = tokio::select! {
                _=cancel.cancelled()=>{stopped="cancelled".into();break;},
                p=reader.conversations(category,cp.cursor.as_deref())=>p,
            };
            match fetched {
                Ok(page) => {
                    db::clear_account_failure(store, account, request_started)?;
                    session_guard::record_success(store, account, request_started)?;
                    if db::commit_conversations(store, account, category, &cp, &page, now()).is_err() {
                        // The page transaction rolled back. Keep its cursor and
                        // persist a sanitized scope failure outside that transaction.
                        db::record_failure(store, account, category, false,
                            "archive page commit failed", retry_at(store, account, &ArchiveError::Transport, now())?)?;
                        stopped = "archive page commit failed".into();
                        break;
                    }
                }
                Err(e) => {
                    let retry = retry_at(store, account, &e, now())?;
                    account_deferred = defer_account(store, account, &e, retry)?;
                    // A cursor-bearing request rejected as bad/gone gets one
                    // bounded reconciliation. This is deliberately not applied
                    // to auth, throttling, or malformed successful responses.
                    if matches!(e, ArchiveError::Status(400 | 410))
                        && db::restart_cursor(store, account, category, false, &cp, now())?
                    {
                        continue;
                    }
                    db::record_failure(
                        store,
                        account,
                        category,
                        false,
                        &e.to_string(),
                        retry,
                    )?;
                    stopped = e.to_string();
                    break;
                }
            }
        } else {
            let pending = db::pending_threads(store, account, category)?;
            if pending.is_empty() {
                stopped = "history_exhausted".into();
                break;
            }
            let Some((thread, cp)) = pending.into_iter().find(|(_, p)| p.retry_at_ms <= now())
            else {
                stopped = "retry_deferred".into();
                break;
            };
            if let Some(stop) = admit(pacer, store, account, cancel, now).await? {
                stopped = stop.reason.into();
                resume_at_ms = stop.resume_at_ms;
                account_deferred |= stop.account_deferred;
                break;
            }
            request_started = now();
            pages += 1;
            let fetched = tokio::select! {
                _=cancel.cancelled()=>{stopped="cancelled".into();break;},
                p=reader.messages(&thread,cp.cursor.as_deref())=>p,
            };
            match fetched {
                Ok(page) => {
                    db::clear_account_failure(store, account, request_started)?;
                    session_guard::record_success(store, account, request_started)?;
                    if db::commit_messages(store, account, &thread, &cp, &page, now()).is_err() {
                        db::record_failure(store, account, &thread, true,
                            "archive page commit failed", retry_at(store, account, &ArchiveError::Transport, now())?)?;
                        // Other threads can still make progress during backoff.
                    }
                }
                Err(e) => {
                    let retry = retry_at(store, account, &e, now())?;
                    account_deferred = defer_account(store, account, &e, retry)?;
                    if matches!(e, ArchiveError::Status(400 | 410))
                        && db::restart_cursor(store, account, &thread, true, &cp, now())?
                    {
                        continue;
                    }
                    db::record_failure(
                        store,
                        account,
                        &thread,
                        true,
                        &e.to_string(),
                        retry,
                    )?;
                    // Session/throttle failures are account-wide. Other failures
                    // leave this thread partial while allowing other threads.
                    if account_deferred {
                        stopped = e.to_string();
                        break;
                    }
                }
            }
        }
    }
    Ok(SyncReport {
        pages,
        stopped,
        account_deferred,
        resume_at_ms,
        coverage: db::coverage(store, account, category)?,
    })
}

fn defer_account(
    store: &Store,
    account: &str,
    error: &ArchiveError,
    retry_at_ms: i64,
) -> anyhow::Result<bool> {
    let wide = matches!(
        error,
        ArchiveError::Auth | ArchiveError::Redirect | ArchiveError::RateLimited(_)
    );
    if wide {
        db::record_account_failure(store, account, &error.to_string(), retry_at_ms)?;
    }
    Ok(wide)
}

/// Inventory obeys the same durable account deferral as mailbox/history reads.
/// None means deferred or cancelled, never an empty accessible mailbox.
pub async fn categories(
    reader: &ArchiveReader,
    store: &Store,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<Vec<String>>> {
    categories_with_clock(reader, store, cancel, &now_ms).await
}

/// Clock injection, as for [`sync_with_clock`]. A paced backfill passes its
/// pacer's clock so discovery lands in the same request ledger timeline.
#[doc(hidden)]
pub async fn categories_with_clock(
    reader: &ArchiveReader,
    store: &Store,
    cancel: &CancellationToken,
    now_ms: &(dyn Fn() -> i64 + Sync),
) -> anyhow::Result<Option<Vec<String>>> {
    let started = now_ms();
    if cancel.is_cancelled() || db::account_retry_at(store, reader.mailbox())? > started {
        return Ok(None);
    }
    backfill_pace::record_request(store, reader.mailbox(), started)?;
    let fetched = tokio::select! {
        _=cancel.cancelled()=>return Ok(None),
        result=reader.categories()=>result,
    };
    match fetched {
        Ok(categories) => {
            db::record_inventory(store, reader.mailbox(), &categories, started)?;
            db::clear_account_failure(store, reader.mailbox(), started)?;
            session_guard::record_success(store, reader.mailbox(), started)?;
            Ok(Some(categories))
        }
        Err(error) => {
            // Inventory is required for every implicit-scope poll. Persist all
            // its failures, including malformed responses and transient I/O.
            let retry = retry_at(store, reader.mailbox(), &error, now_ms())?;
            db::record_account_failure(store, reader.mailbox(), &error.to_string(), retry)?;
            Err(error.into())
        }
    }
}
