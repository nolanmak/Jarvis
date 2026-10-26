//! Durable guard facts, separate from failed dispatches. No message bodies,
//! subjects, model text, or arbitrary error strings enter these observations.
use crate::{Store, StoreResult};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Copy, Debug)]
pub enum EmailGuard {
    AlreadyComplete,
    OpenAction,
    PrefilterSkip,
    AutomatedSender,
    AlreadyReplied,
    EventBlast,
    CcOnly,
    MeetingInvite,
}
impl EmailGuard {
    fn fields(self) -> (&'static str, &'static str) {
        match self {
            Self::AlreadyComplete => ("already_complete", "no_new_dispatch"),
            Self::OpenAction => ("open_action", "defer_to_existing_action"),
            Self::PrefilterSkip => ("prefilter_skip", "skip"),
            Self::AutomatedSender => ("automated_sender", "skip"),
            Self::AlreadyReplied => ("already_replied", "skip"),
            Self::EventBlast => ("event_blast", "ingest_only"),
            Self::CcOnly => ("cc_only", "flag_without_draft"),
            Self::MeetingInvite => ("meeting_invite", "flag_without_draft"),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailGuardObservation {
    pub message_id: String,
    pub action_id: String,
    pub attempt_n: i64,
    pub guard: String,
    pub disposition: String,
    pub observed_at_ms: i64,
}
impl Store {
    pub fn record_email_guard(
        &self,
        message: &str,
        action: Option<&str>,
        guard: EmailGuard,
    ) -> StoreResult<()> {
        self.with_conn(|conn| {
            // The retry controller increments before entering the shared guard
            // chain. Bind only to this message's real action, never borrowed IDs.
            let attempt = match action {
                Some(id) => conn.query_row("SELECT COALESCE(retryCount,0)+1 FROM actions WHERE id=?1 AND messageId=?2",params![id,message],|r| r.get::<_,i64>(0)).optional()?.ok_or(rusqlite::Error::QueryReturnedNoRows)?,
                None => 1,
            };
            let (guard, disposition) = guard.fields();
            conn.execute("INSERT OR IGNORE INTO email_guard_observations(message_id,action_id,attempt_n,guard,disposition,observed_at_ms) VALUES(?1,?2,?3,?4,?5,?6)",params![message,action.unwrap_or(""),attempt,guard,disposition,chrono::Utc::now().timestamp_millis()])?;
            Ok(())
        })
    }
    pub fn email_guards_since(
        &self,
        since_ms: i64,
        limit: usize,
    ) -> StoreResult<Vec<EmailGuardObservation>> {
        self.with_conn(|conn| {
            let mut stmt=conn.prepare("SELECT message_id,action_id,attempt_n,guard,disposition,observed_at_ms FROM email_guard_observations WHERE observed_at_ms>=?1 ORDER BY observed_at_ms DESC,id DESC LIMIT ?2")?;
            let rows=stmt.query_map(params![since_ms,limit.min(1000) as i64],|r| Ok(EmailGuardObservation {message_id:r.get(0)?,action_id:r.get(1)?,attempt_n:r.get(2)?,guard:r.get(3)?,disposition:r.get(4)?,observed_at_ms:r.get(5)?}))?;
            rows.collect()
        })
    }
    pub fn email_guard_history(&self, message: &str) -> StoreResult<String> {
        self.with_conn(|conn| {
            let mut stmt=conn.prepare("SELECT attempt_n,guard,disposition FROM email_guard_observations WHERE message_id=?1 ORDER BY id DESC LIMIT 25")?;
            let rows=stmt.query_map([message],|r| Ok(format!("attempt {}: {} => {}",r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.join("; "))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guard_memory_survives_restart_is_idempotent_and_never_creates_retry_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = Store::open(&path).unwrap();
        store
            .record_email_guard("synthetic", None, EmailGuard::EventBlast)
            .unwrap();
        store
            .record_email_guard("synthetic", None, EmailGuard::EventBlast)
            .unwrap();
        assert!(store
            .record_email_guard("synthetic", Some("unrelated"), EmailGuard::CcOnly)
            .is_err());
        drop(store);
        let store = Store::open(&path).unwrap();
        let observations = store.email_guards_since(0, 25).unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].disposition, "ingest_only");
        assert_eq!(observations[0].attempt_n, 1);
        assert!(store
            .email_guard_history("synthetic")
            .unwrap()
            .contains("event_blast => ingest_only"));
        assert!(store.email_retry_failures_since(0, 25).unwrap().is_empty());
        store
            .with_conn(|c| {
                assert_eq!(
                    c.query_row("SELECT count(*) FROM actions", [], |r| r.get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .unwrap();
    }
}
