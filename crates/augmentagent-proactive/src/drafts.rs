//! Channel-draft lifecycle for high-stakes outbound (email, linkedin,
//! slack DMs). Caller decides which channels go through drafts; others
//! still send directly.
//!
//! State machine:
//!
//! ```text
//!   create_draft → Pending ─approve_draft→ Approved ─mark_published→ Published
//!                     │                        │
//!                     └──── discard_draft ─────┴──→ Discarded
//! ```
//!
//! Transitions are enforced here (not by SQL CHECK) so adding a new status
//! later is a code change, not a schema migration. Terminal states
//! (`Published`, `Discarded`) reject further transitions and return `false`
//! from the mutator; callers can distinguish "no such draft" (Err) from
//! "draft is past the gate" (Ok(false)).
//!
//! The `payload_json` column is opaque to this crate — channel crates own
//! the shape. Mirror of [`ProactiveStore`](crate::ProactiveStore): we layer
//! a typed CRUD trait onto [`augmentagent_store::Store`] and share the
//! daemon's single WAL connection.

use augmentagent_store::{rusqlite, Store, StoreResult};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

/// Lifecycle state of a draft. Stored as a TEXT enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftStatus {
    Pending,
    Approved,
    Published,
    Discarded,
}

impl DraftStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Published => "published",
            Self::Discarded => "discarded",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "approved" => Self::Approved,
            "published" => Self::Published,
            "discarded" => Self::Discarded,
            _ => Self::Pending,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Published | Self::Discarded)
    }
}

/// A stored draft row with lifecycle timestamps + optional outcome.
#[derive(Debug, Clone)]
pub struct StoredDraft {
    pub id: String,
    pub target_channel: String,
    pub payload_json: String,
    pub status: DraftStatus,
    pub note: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub approved_at_ms: Option<i64>,
    pub published_at_ms: Option<i64>,
    pub discarded_at_ms: Option<i64>,
    pub publish_result_json: Option<String>,
    pub error_message: Option<String>,
}

/// Draft-lifecycle CRUD surface, implemented for [`Store`].
pub trait DraftStore {
    /// Insert a new draft in `Pending`. Returns the assigned UUID.
    fn create_draft(
        &self,
        target_channel: &str,
        payload_json: &str,
        note: Option<&str>,
    ) -> StoreResult<String>;

    /// Fetch a single draft by id.
    fn get_draft(&self, id: &str) -> StoreResult<Option<StoredDraft>>;

    /// List drafts, optionally scoped by status / target channel.
    /// Newest-first by `created_at_ms`. `limit` caps the result.
    fn list_drafts(
        &self,
        status: Option<DraftStatus>,
        target_channel: Option<&str>,
        limit: u32,
    ) -> StoreResult<Vec<StoredDraft>>;

    /// Pending → Approved. Returns `false` if the draft is missing or
    /// already past Pending.
    fn approve_draft(&self, id: &str) -> StoreResult<bool>;

    /// Pending|Approved → Discarded. Returns `false` if missing or terminal.
    fn discard_draft(&self, id: &str) -> StoreResult<bool>;

    /// Approved → Published, recording the channel's response.
    /// Returns `false` if the draft is not in Approved.
    fn mark_published(&self, id: &str, result_json: &str) -> StoreResult<bool>;

    /// Records a publish failure. Status stays `Approved` so the caller
    /// can retry; only the `error_message` and `updated_at_ms` move.
    fn mark_publish_failed(&self, id: &str, error: &str) -> StoreResult<bool>;

    /// Auto-discard any draft that's been stuck in `Pending` longer than
    /// `ttl_ms`. Returns the count discarded. Intended to be called from
    /// the proactive runner on a cron.
    fn expire_stale_drafts(&self, now_ms: i64, ttl_ms: i64) -> StoreResult<usize>;
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredDraft> {
    let status_str: String = row.get("status")?;
    Ok(StoredDraft {
        id: row.get("id")?,
        target_channel: row.get("target_channel")?,
        payload_json: row.get("payload_json")?,
        status: DraftStatus::parse(&status_str),
        note: row.get("note")?,
        created_at_ms: row.get("created_at_ms")?,
        updated_at_ms: row.get("updated_at_ms")?,
        approved_at_ms: row.get("approved_at_ms")?,
        published_at_ms: row.get("published_at_ms")?,
        discarded_at_ms: row.get("discarded_at_ms")?,
        publish_result_json: row.get("publish_result_json")?,
        error_message: row.get("error_message")?,
    })
}

impl DraftStore for Store {
    fn create_draft(
        &self,
        target_channel: &str,
        payload_json: &str,
        note: Option<&str>,
    ) -> StoreResult<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO channel_drafts \
                   (id, target_channel, payload_json, status, note, \
                    created_at_ms, updated_at_ms) \
                 VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?5)",
                params![id, target_channel, payload_json, note, now],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    fn get_draft(&self, id: &str) -> StoreResult<Option<StoredDraft>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, target_channel, payload_json, status, note, \
                        created_at_ms, updated_at_ms, approved_at_ms, \
                        published_at_ms, discarded_at_ms, \
                        publish_result_json, error_message \
                 FROM channel_drafts WHERE id = ?1",
                params![id],
                row_to_draft,
            )
            .optional()
        })
    }

    fn list_drafts(
        &self,
        status: Option<DraftStatus>,
        target_channel: Option<&str>,
        limit: u32,
    ) -> StoreResult<Vec<StoredDraft>> {
        self.with_conn(|c| {
            let mut sql = String::from(
                "SELECT id, target_channel, payload_json, status, note, \
                        created_at_ms, updated_at_ms, approved_at_ms, \
                        published_at_ms, discarded_at_ms, \
                        publish_result_json, error_message \
                 FROM channel_drafts WHERE 1=1",
            );
            let mut binds: Vec<String> = Vec::new();
            if let Some(s) = status {
                sql.push_str(" AND status = ?");
                binds.push(s.as_str().to_string());
            }
            if let Some(t) = target_channel {
                sql.push_str(" AND target_channel = ?");
                binds.push(t.to_string());
            }
            sql.push_str(" ORDER BY created_at_ms DESC LIMIT ?");
            binds.push(limit.to_string());

            let mut stmt = c.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
            let rows = stmt
                .query_map(params.as_slice(), row_to_draft)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn approve_draft(&self, id: &str) -> StoreResult<bool> {
        let now = now_ms();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_drafts \
                    SET status = 'approved', \
                        approved_at_ms = ?2, \
                        updated_at_ms = ?2 \
                  WHERE id = ?1 AND status = 'pending'",
                params![id, now],
            )?;
            Ok(n > 0)
        })
    }

    fn discard_draft(&self, id: &str) -> StoreResult<bool> {
        let now = now_ms();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_drafts \
                    SET status = 'discarded', \
                        discarded_at_ms = ?2, \
                        updated_at_ms = ?2 \
                  WHERE id = ?1 AND status IN ('pending', 'approved')",
                params![id, now],
            )?;
            Ok(n > 0)
        })
    }

    fn mark_published(&self, id: &str, result_json: &str) -> StoreResult<bool> {
        let now = now_ms();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_drafts \
                    SET status = 'published', \
                        published_at_ms = ?2, \
                        updated_at_ms = ?2, \
                        publish_result_json = ?3 \
                  WHERE id = ?1 AND status = 'approved'",
                params![id, now, result_json],
            )?;
            Ok(n > 0)
        })
    }

    fn mark_publish_failed(&self, id: &str, error: &str) -> StoreResult<bool> {
        let now = now_ms();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_drafts \
                    SET error_message = ?3, \
                        updated_at_ms = ?2 \
                  WHERE id = ?1 AND status = 'approved'",
                params![id, now, error],
            )?;
            Ok(n > 0)
        })
    }

    fn expire_stale_drafts(&self, now_ms: i64, ttl_ms: i64) -> StoreResult<usize> {
        let cutoff = now_ms - ttl_ms;
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_drafts \
                    SET status = 'discarded', \
                        discarded_at_ms = ?1, \
                        updated_at_ms = ?1, \
                        error_message = 'auto-expired (TTL)' \
                  WHERE status = 'pending' AND created_at_ms < ?2",
                params![now_ms, cutoff],
            )?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_store;

    #[test]
    fn create_then_get() {
        let (_d, s) = test_store();
        let id = s
            .create_draft("email", r#"{"to":"x@y","body":"hi"}"#, Some("hand-typed"))
            .unwrap();
        let d = s.get_draft(&id).unwrap().expect("present");
        assert_eq!(d.target_channel, "email");
        assert_eq!(d.status, DraftStatus::Pending);
        assert_eq!(d.note.as_deref(), Some("hand-typed"));
        assert!(d.approved_at_ms.is_none());
    }

    #[test]
    fn approve_then_publish_happy_path() {
        let (_d, s) = test_store();
        let id = s.create_draft("linkedin", r#"{"text":"hello"}"#, None).unwrap();
        assert!(s.approve_draft(&id).unwrap());
        assert!(s.mark_published(&id, r#"{"urn":"li:post:123"}"#).unwrap());
        let d = s.get_draft(&id).unwrap().unwrap();
        assert_eq!(d.status, DraftStatus::Published);
        assert!(d.published_at_ms.is_some());
        assert_eq!(d.publish_result_json.as_deref(), Some(r#"{"urn":"li:post:123"}"#));
    }

    #[test]
    fn cannot_publish_without_approval() {
        let (_d, s) = test_store();
        let id = s.create_draft("slack", "{}", None).unwrap();
        assert!(!s.mark_published(&id, "{}").unwrap());
        let d = s.get_draft(&id).unwrap().unwrap();
        assert_eq!(d.status, DraftStatus::Pending);
    }

    #[test]
    fn approve_twice_returns_false() {
        let (_d, s) = test_store();
        let id = s.create_draft("email", "{}", None).unwrap();
        assert!(s.approve_draft(&id).unwrap());
        assert!(!s.approve_draft(&id).unwrap());
    }

    #[test]
    fn discard_from_pending() {
        let (_d, s) = test_store();
        let id = s.create_draft("email", "{}", None).unwrap();
        assert!(s.discard_draft(&id).unwrap());
        assert_eq!(
            s.get_draft(&id).unwrap().unwrap().status,
            DraftStatus::Discarded
        );
    }

    #[test]
    fn discard_from_approved() {
        let (_d, s) = test_store();
        let id = s.create_draft("email", "{}", None).unwrap();
        s.approve_draft(&id).unwrap();
        assert!(s.discard_draft(&id).unwrap());
    }

    #[test]
    fn cannot_discard_published() {
        let (_d, s) = test_store();
        let id = s.create_draft("email", "{}", None).unwrap();
        s.approve_draft(&id).unwrap();
        s.mark_published(&id, "{}").unwrap();
        assert!(!s.discard_draft(&id).unwrap());
    }

    #[test]
    fn publish_failure_keeps_approved() {
        let (_d, s) = test_store();
        let id = s.create_draft("email", "{}", None).unwrap();
        s.approve_draft(&id).unwrap();
        assert!(s.mark_publish_failed(&id, "smtp 421").unwrap());
        let d = s.get_draft(&id).unwrap().unwrap();
        assert_eq!(d.status, DraftStatus::Approved);
        assert_eq!(d.error_message.as_deref(), Some("smtp 421"));
    }

    #[test]
    fn list_filters_by_status_and_channel() {
        let (_d, s) = test_store();
        let _ = s.create_draft("email", "{}", None).unwrap();
        let id2 = s.create_draft("linkedin", "{}", None).unwrap();
        s.approve_draft(&id2).unwrap();
        let approved = s.list_drafts(Some(DraftStatus::Approved), None, 10).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id, id2);
        let pending_email = s
            .list_drafts(Some(DraftStatus::Pending), Some("email"), 10)
            .unwrap();
        assert_eq!(pending_email.len(), 1);
        assert_eq!(pending_email[0].target_channel, "email");
    }

    #[test]
    fn expire_stale_only_pending() {
        let (_d, s) = test_store();
        let now = now_ms();
        let ttl = 1_000;
        let stale = s.create_draft("email", "{}", None).unwrap();
        s.with_conn(|c| {
            c.execute(
                "UPDATE channel_drafts SET created_at_ms = ?2, updated_at_ms = ?2 WHERE id = ?1",
                params![stale, now - 10_000],
            )?;
            Ok(())
        })
        .unwrap();

        let fresh = s.create_draft("email", "{}", None).unwrap();
        let approved_old = s.create_draft("email", "{}", None).unwrap();
        s.with_conn(|c| {
            c.execute(
                "UPDATE channel_drafts SET created_at_ms = ?2, updated_at_ms = ?2 WHERE id = ?1",
                params![approved_old, now - 10_000],
            )?;
            Ok(())
        })
        .unwrap();
        s.approve_draft(&approved_old).unwrap();

        let n = s.expire_stale_drafts(now, ttl).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            s.get_draft(&stale).unwrap().unwrap().status,
            DraftStatus::Discarded
        );
        assert_eq!(
            s.get_draft(&fresh).unwrap().unwrap().status,
            DraftStatus::Pending
        );
        assert_eq!(
            s.get_draft(&approved_old).unwrap().unwrap().status,
            DraftStatus::Approved,
            "non-pending drafts are not auto-expired"
        );
    }
}
