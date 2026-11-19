//! Backfill reads slowly on purpose. Live polling is spaced hours apart; a
//! history import that fires its page requests back to back is the one burst
//! of traffic this channel can produce.
//!
//! Every archive request, live or backfill, is written to
//! `linkedin_archive_requests`. Backfill waits a jittered delay after the
//! last request of either kind and stops when it has spent its own hourly or
//! daily budget for the account. Live reads are never delayed, refused or
//! charged to that budget: backfill cannot take a request the live poll
//! needs, and the resume time it reports does not move when a poll runs.
use augmentagent_store::{
    rusqlite::{params, OptionalExtension},
    Store,
};
use std::{future::Future, pin::Pin};
use tokio_util::sync::CancellationToken;

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pacing {
    /// Minimum time between two requests.
    pub delay_ms: i64,
    /// Up to this much is added to each delay.
    pub jitter_ms: i64,
    /// Backfill requests per account in any sliding hour and day.
    pub hourly: i64,
    pub daily: i64,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            delay_ms: 5000,
            jitter_ms: 5000,
            hourly: 60,
            daily: 300,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Go,
    /// The budget is spent until this time; no request may be made.
    Budget {
        resume_at_ms: i64,
    },
    Cancelled,
}

type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Clock, sleeper and jitter are injected so tests never wait.
pub struct Pacer {
    pacing: Pacing,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    sleep: Box<dyn Fn(i64) -> Sleep + Send + Sync>,
    jitter: Box<dyn Fn(i64) -> i64 + Send + Sync>,
}

impl Pacer {
    pub fn new(pacing: Pacing) -> Self {
        Self::with_parts(
            pacing,
            || (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            |ms| {
                Box::pin(tokio::time::sleep(std::time::Duration::from_millis(
                    ms.max(0) as u64,
                )))
            },
            // The clock's nanosecond tail, as the live poll jitter does.
            |max| i64::from(time::OffsetDateTime::now_utc().nanosecond()) % (max + 1),
        )
    }

    /// `sleep` gets milliseconds; `jitter` returns a value in `0..=max`.
    pub fn with_parts(
        pacing: Pacing,
        now: impl Fn() -> i64 + Send + Sync + 'static,
        sleep: impl Fn(i64) -> Sleep + Send + Sync + 'static,
        jitter: impl Fn(i64) -> i64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            pacing,
            now: Box::new(now),
            sleep: Box::new(sleep),
            jitter: Box::new(jitter),
        }
    }

    pub fn now(&self) -> i64 {
        (self.now)()
    }

    /// Wait until the next backfill request may go out. On `Go` the caller
    /// records the request with [`record_request`] when it sends it.
    pub async fn admit(
        &self,
        store: &Store,
        account: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Admit> {
        let now = self.now();
        if let Some(resume_at_ms) = budget_resume_at(store, account, &self.pacing, now)? {
            return Ok(Admit::Budget { resume_at_ms });
        }
        if let Some(last) = last_request(store, account)? {
            let jitter = if self.pacing.jitter_ms > 0 {
                (self.jitter)(self.pacing.jitter_ms).clamp(0, self.pacing.jitter_ms)
            } else {
                0
            };
            let gap = self.pacing.delay_ms.max(0).saturating_add(jitter);
            // A ledger row from a clock that ran ahead must not stall the run.
            let wait = last.saturating_add(gap).saturating_sub(now).min(gap);
            if wait > 0 {
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(Admit::Cancelled),
                    _ = (self.sleep)(wait) => {}
                }
            }
        }
        Ok(Admit::Go)
    }
}

/// Record one outgoing archive request and drop rows older than a day.
/// Only `backfill` requests count against the budget.
pub fn record_request(
    store: &Store,
    account: &str,
    now: i64,
    backfill: bool,
) -> anyhow::Result<()> {
    store.with_conn(|c| {
        c.execute(
            "DELETE FROM linkedin_archive_requests WHERE account=?1 AND at_ms<=?2",
            params![account, now.saturating_sub(DAY_MS)],
        )?;
        c.execute(
            "INSERT INTO linkedin_archive_requests(account,at_ms,backfill) VALUES(?1,?2,?3)",
            params![account, now, backfill],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn last_request(store: &Store, account: &str) -> anyhow::Result<Option<i64>> {
    Ok(store.with_conn(|c| {
        c.query_row(
            "SELECT MAX(at_ms) FROM linkedin_archive_requests WHERE account=?1",
            [account],
            |r| r.get(0),
        )
    })?)
}

/// None while backfill may still read; otherwise when the spent window
/// reopens (the later of the two when both are spent).
pub fn budget_resume_at(
    store: &Store,
    account: &str,
    pacing: &Pacing,
    now: i64,
) -> anyhow::Result<Option<i64>> {
    let mut resume: Option<i64> = None;
    for (limit, window) in [(pacing.hourly, HOUR_MS), (pacing.daily, DAY_MS)] {
        // The request that must age out before one more fits in the window.
        let blocking: Option<i64> = store.with_conn(|c| {
            c.query_row(
                "SELECT at_ms FROM linkedin_archive_requests
                 WHERE account=?1 AND at_ms>?2 AND backfill=1
                 ORDER BY at_ms DESC LIMIT 1 OFFSET ?3",
                params![account, now.saturating_sub(window), limit.max(1) - 1],
                |r| r.get(0),
            )
            .optional()
        })?;
        if let Some(at) = blocking {
            resume = resume.max(Some(at.saturating_add(window)));
        }
    }
    Ok(resume)
}
