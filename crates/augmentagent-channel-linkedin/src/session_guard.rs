//! A session that needs a human to log in again: tell the owner, stop
//! replaying the dead cookie, and say so once when reads work again.
//!
//! State lives in `linkedin_session_health`, so notices and backoff survive
//! restarts and are shared by the daemon and the CLI.
use crate::archive::ArchiveError;
use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{
    rusqlite::{params, OptionalExtension},
    Email, Store,
};

/// First retry after a session failure; doubles on each further failure.
pub const BASE_BACKOFF_MS: i64 = 60 * 1000;
/// Longest pause between attempts with a session LinkedIn keeps rejecting.
pub const MAX_BACKOFF_MS: i64 = 6 * 60 * 60 * 1000;
/// A dead session is repeated to the owner at most this often.
pub const REPEAT_NOTICE_MS: i64 = 24 * 60 * 60 * 1000;
pub const LOGIN_COMMAND: &str = "augmentagent linkedin login --cookies-json <file>";

/// Failures only a fresh login can fix. Throttling and transport errors heal
/// by themselves and are not reported as a dead session.
pub fn is_session_failure(error: &ArchiveError) -> bool {
    matches!(error, ArchiveError::Auth | ArchiveError::Redirect)
}

pub fn backoff_ms(failures: i64) -> i64 {
    let doublings = failures.saturating_sub(1).clamp(0, 30) as u32;
    BASE_BACKOFF_MS
        .saturating_mul(1_i64 << doublings)
        .min(MAX_BACKOFF_MS)
}

/// Count one rejected read and return when the account may be tried again.
pub fn record_failure(store: &Store, account: &str, error: &str, now: i64) -> anyhow::Result<i64> {
    let failures: i64 = store.with_conn(|c| {
        c.execute(
            "INSERT INTO linkedin_session_health(account,failures,dead_since_ms,last_error) VALUES(?1,1,?3,?2)
             ON CONFLICT(account) DO UPDATE SET failures=failures+1,
               dead_since_ms=COALESCE(dead_since_ms,?3),last_error=?2,recovery_pending=0",
            params![account, error, now],
        )?;
        c.query_row(
            "SELECT failures FROM linkedin_session_health WHERE account=?1",
            [account],
            |r| r.get(0),
        )
    })?;
    Ok(now.saturating_add(backoff_ms(failures)))
}

/// A read that began at `request_started_ms` succeeded. A request older than
/// the outage cannot end it.
pub fn record_success(store: &Store, account: &str, request_started_ms: i64) -> anyhow::Result<()> {
    store.with_conn(|c| {
        c.execute(
            "UPDATE linkedin_session_health SET failures=0,dead_since_ms=NULL,last_error=NULL,
               recovery_pending=CASE WHEN last_notice_ms IS NOT NULL THEN 1 ELSE 0 END
             WHERE account=?1 AND dead_since_ms IS NOT NULL AND dead_since_ms<=?2",
            params![account, request_started_ms],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// A new login makes the next read go out at once. The session stays reported
/// as dead until that read succeeds. A throttle is never lifted here.
pub fn clear_pause_after_login(store: &Store, account: &str) -> anyhow::Result<bool> {
    let (auth, redirect) = (
        ArchiveError::Auth.to_string(),
        ArchiveError::Redirect.to_string(),
    );
    Ok(store.with_conn(|c| {
        let tx = c.unchecked_transaction()?;
        let dead = tx.execute(
            "UPDATE linkedin_session_health SET failures=0 WHERE account=?1 AND dead_since_ms IS NOT NULL",
            [account],
        )?;
        if dead == 0 {
            return Ok(false);
        }
        tx.execute(
            "UPDATE linkedin_archive_accounts SET last_error=NULL,retry_at_ms=0 WHERE account=?1 AND last_error IN (?2,?3)",
            params![account, auth, redirect],
        )?;
        for table in ["linkedin_archive_mailboxes", "linkedin_archive_conversations"] {
            tx.execute(
                &format!("UPDATE {table} SET retry_at_ms=0 WHERE account=?1 AND last_error IN (?2,?3)"),
                params![account, auth, redirect],
            )?;
        }
        tx.commit()?;
        Ok(true)
    })?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadSession {
    pub account: String,
    pub since_ms: i64,
    pub error: String,
}

/// Accounts whose session is rejected right now, for `doctor` and the digest.
pub fn dead_sessions(store: &Store) -> anyhow::Result<Vec<DeadSession>> {
    Ok(store.with_conn(|c| {
        let mut q = c.prepare(
            "SELECT account,dead_since_ms,COALESCE(last_error,'') FROM linkedin_session_health
             WHERE dead_since_ms IS NOT NULL ORDER BY account",
        )?;
        let rows = q
            .query_map([], |r| {
                Ok(DeadSession {
                    account: r.get(0)?,
                    since_ms: r.get(1)?,
                    error: r.get(2)?,
                })
            })?
            .collect();
        rows
    })?)
}

fn utc(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000))
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| format!("{ms} ms"))
}

/// One line for the daily digest while any session is dead.
pub fn digest_line(store: &Store) -> anyhow::Result<Option<String>> {
    let dead = dead_sessions(store)?;
    if dead.is_empty() {
        return Ok(None);
    }
    let accounts = dead
        .iter()
        .map(|d| format!("{} since {}", d.account, utc(d.since_ms)))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Some(format!(
        "LinkedIn is not being read ({accounts}). Run `{LOGIN_COMMAND}` with fresh cookies."
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Dead {
        since_ms: i64,
        error: String,
        reminder: bool,
    },
    Recovered,
}

impl Notice {
    pub fn subject(&self) -> &'static str {
        match self {
            Notice::Dead {
                reminder: false, ..
            } => "LinkedIn stopped reading",
            Notice::Dead { reminder: true, .. } => "LinkedIn is still not reading",
            Notice::Recovered => "LinkedIn is reading again",
        }
    }
    pub fn text(&self, account: &str) -> String {
        match self {
            Notice::Dead { since_ms, error, .. } => format!(
                "LinkedIn messages for {account} are not being read since {}: {error}. \
                 New DMs are not triaged and message search is missing this channel. \
                 Harvest fresh cookies and run `{LOGIN_COMMAND}`. \
                 This repeats once a day until a read succeeds.",
                utc(*since_ms)
            ),
            Notice::Recovered => format!(
                "LinkedIn is reading again for {account}: a read succeeded with the current session."
            ),
        }
    }
}

/// Decide whether a notice is due and reserve it, so two processes polling
/// the same account cannot both post. Returns the value to restore if the
/// post fails.
fn claim(store: &Store, account: &str, now: i64) -> anyhow::Result<Option<(Notice, Option<i64>)>> {
    Ok(store.with_conn(|c| {
        let tx = c.unchecked_transaction()?;
        let row: Option<(Option<i64>, Option<String>, Option<i64>, bool)> = tx
            .query_row(
                "SELECT dead_since_ms,last_error,last_notice_ms,recovery_pending FROM linkedin_session_health WHERE account=?1",
                [account],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((dead_since, error, last_notice, recovery_pending)) = row else {
            return Ok(None);
        };
        let notice = match dead_since {
            Some(since_ms) => {
                if last_notice.is_some_and(|sent| now.saturating_sub(sent) < REPEAT_NOTICE_MS) {
                    return Ok(None);
                }
                tx.execute(
                    "UPDATE linkedin_session_health SET last_notice_ms=?2 WHERE account=?1",
                    params![account, now],
                )?;
                Notice::Dead {
                    since_ms,
                    error: error.unwrap_or_default(),
                    reminder: last_notice.is_some(),
                }
            }
            None if recovery_pending => {
                tx.execute(
                    "UPDATE linkedin_session_health SET recovery_pending=0,last_notice_ms=NULL WHERE account=?1",
                    [account],
                )?;
                Notice::Recovered
            }
            None => return Ok(None),
        };
        tx.commit()?;
        Ok(Some((notice, last_notice)))
    })?)
}

fn release(
    store: &Store,
    account: &str,
    notice: &Notice,
    previous: Option<i64>,
) -> anyhow::Result<()> {
    let recovered = matches!(notice, Notice::Recovered);
    store.with_conn(|c| {
        c.execute(
            "UPDATE linkedin_session_health SET last_notice_ms=?2,recovery_pending=MAX(recovery_pending,?3) WHERE account=?1",
            params![account, previous, recovered],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn notice_email(notice: &Notice, text: &str, now: i64) -> Email {
    Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: format!("linkedin-session:{now}"),
        thread_id: None,
        from: "linkedin".into(),
        subject: notice.subject().into(),
        body: text.into(),
        date: String::new(),
        account_entity_id: None,
        platform: "linkedin".into(),
        kind: "session_notice".into(),
    }
}

/// Post the notice that is due for `account`, if any: the first one when the
/// session dies, a reminder at most once a day while it stays dead, and one
/// when a read succeeds again. An undelivered notice stays due.
pub async fn notify(
    store: &Store,
    broker: &dyn ApprovalBroker,
    account: &str,
    now: i64,
) -> anyhow::Result<Option<Notice>> {
    let Some((notice, previous)) = claim(store, account, now)? else {
        return Ok(None);
    };
    let text = notice.text(account);
    if let Err(error) = broker
        .post_flag_notice(&notice_email(&notice, &text, now), &text)
        .await
    {
        release(store, account, &notice, previous)?;
        anyhow::bail!("LinkedIn session notice not delivered: {error}");
    }
    Ok(Some(notice))
}

/// `notify` for poll loops: a notice problem never fails the poll.
pub async fn notify_after_poll(store: &Store, broker: &dyn ApprovalBroker, account: &str) {
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    match notify(store, broker, account, now).await {
        Ok(Some(notice)) => tracing::warn!("linkedin session notice posted: {}", notice.subject()),
        Ok(None) => {}
        Err(error) => tracing::warn!("{error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_a_minute_and_stops_at_the_cap() {
        assert_eq!(backoff_ms(0), BASE_BACKOFF_MS);
        assert_eq!(backoff_ms(1), 60_000);
        assert_eq!(backoff_ms(2), 120_000);
        assert_eq!(backoff_ms(9), 256 * 60_000);
        assert_eq!(backoff_ms(10), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(i64::MAX), MAX_BACKOFF_MS);
    }

    #[test]
    fn notices_name_the_account_and_fit_one_discord_notice() {
        let dead = Notice::Dead {
            since_ms: 1_700_000_000_000,
            error: ArchiveError::Redirect.to_string(),
            reminder: false,
        };
        let text = dead.text("urn:li:fsd_profile:OWNER");
        assert!(text.contains("2023-11-14T22:13:20Z"), "{text}");
        assert!(text.contains(LOGIN_COMMAND));
        // The approval channel truncates a notice reason at 500 characters.
        assert!(text.chars().count() <= 500, "{}", text.chars().count());
    }
}
