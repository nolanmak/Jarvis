//! Did the owner answer this person? (#2285, backed by #2284 later.)
//!
//! The daily sweep asks this of every "unanswered person" candidate before
//! the model sees it. [`StoreReplyEvidence`] answers from what the store
//! already records; the cross-channel reply detection of #2284 plugs in
//! through [`ReplyEvidence`] (`HeartbeatRunner::with_reply_evidence`)
//! without touching the collectors.

use std::sync::Arc;

use augmentagent_store::{rusqlite, Store};
use rusqlite::params;

/// One inbound message the sweep thinks may be unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundRef {
    pub message_id: String,
    pub platform: String,
    pub conversation_id: String,
    /// `message_index.sender_handle`, e.g. `phone:+1...` or `email:...`.
    pub sender_handle: String,
    pub at_ms: i64,
}

pub trait ReplyEvidence: Send + Sync {
    /// True when the owner wrote back to this conversation, or to the same
    /// person on any channel, after `inbound.at_ms`.
    fn owner_replied_since(&self, inbound: &InboundRef) -> bool;
}

/// What the store knows today:
/// - an owner message (`message_index.from_me`) later in the same
///   conversation;
/// - a Gmail send in the same thread, by the owner (`outbound_thread_log`)
///   or the daemon (`self_sent_messages`);
/// - an owner message on any channel to the same person, matched by handle
///   or through `message_people`.
///
/// A query that fails (a table missing on a fresh database) counts as no
/// evidence for that check only.
pub struct StoreReplyEvidence {
    store: Arc<Store>,
}

impl StoreReplyEvidence {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn exists(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> bool {
        self.store
            .with_conn(|c| c.query_row(sql, args, |r| r.get::<_, bool>(0)))
            .unwrap_or(false)
    }
}

impl ReplyEvidence for StoreReplyEvidence {
    fn owner_replied_since(&self, m: &InboundRef) -> bool {
        let same_conversation = self.exists(
            "SELECT EXISTS (SELECT 1 FROM message_index \
             WHERE conversation_id = ?1 AND from_me = 1 AND ts_ms > ?2)",
            params![m.conversation_id, m.at_ms],
        );
        if same_conversation {
            return true;
        }
        let same_thread = self.exists(
            "SELECT EXISTS (SELECT 1 FROM outbound_thread_log WHERE thread_id = ?1 AND sent_at_ms > ?2)",
            params![m.conversation_id, m.at_ms],
        ) || self.exists(
            "SELECT EXISTS (SELECT 1 FROM self_sent_messages WHERE thread_id = ?1 AND sent_at_ms > ?2)",
            params![m.conversation_id, m.at_ms],
        );
        if same_thread {
            return true;
        }
        self.exists(
            "SELECT EXISTS (SELECT 1 FROM message_index o \
             WHERE o.from_me = 1 AND o.ts_ms > ?2 AND o.counterpart_handle IS NOT NULL \
               AND (o.counterpart_handle = ?1 OR o.counterpart_handle IN ( \
                     SELECT p.handle FROM message_people p WHERE p.person_key = \
                       (SELECT q.person_key FROM message_people q WHERE q.handle = ?1))))",
            params![m.sender_handle, m.at_ms],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Arc<Store>) {
        let d = tempfile::tempdir().unwrap();
        let s = Arc::new(Store::open(d.path().join("data.db")).unwrap());
        (d, s)
    }

    fn index(
        s: &Store,
        id: &str,
        conv: &str,
        sender: &str,
        counterpart: Option<&str>,
        me: bool,
        ts: i64,
    ) {
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO message_index (message_id, platform, conv_kind, conversation_id, \
                 sender_handle, counterpart_handle, from_me, ts_ms, has_attachment, extractor_version) \
                 VALUES (?1, 'imessage', 'dm', ?2, ?3, ?4, ?5, ?6, 0, 1)",
                params![id, conv, sender, counterpart, me, ts],
            )
        })
        .unwrap();
    }

    fn inbound(conv: &str, sender: &str) -> InboundRef {
        InboundRef {
            message_id: "in-1".into(),
            platform: "imessage".into(),
            conversation_id: conv.into(),
            sender_handle: sender.into(),
            at_ms: 1_000,
        }
    }

    #[test]
    fn a_later_owner_message_in_the_conversation_is_a_reply() {
        let (_d, s) = store();
        let ev = StoreReplyEvidence::new(Arc::clone(&s));
        let m = inbound("conv-a", "phone:+15550000001");
        assert!(!ev.owner_replied_since(&m));
        index(&s, "old", "conv-a", "me", None, true, 500);
        assert!(
            !ev.owner_replied_since(&m),
            "earlier owner message is not a reply"
        );
        index(&s, "new", "conv-a", "me", None, true, 2_000);
        assert!(ev.owner_replied_since(&m));
    }

    #[test]
    fn a_reply_to_the_same_person_on_another_channel_counts() {
        let (_d, s) = store();
        let ev = StoreReplyEvidence::new(Arc::clone(&s));
        let m = inbound("conv-a", "phone:+15550000001");
        s.with_conn(|c| {
            c.execute_batch(
                "INSERT INTO message_people VALUES ('phone:+15550000001', 'sample-person'),
                                                   ('email:sample@example.com', 'sample-person');",
            )
        })
        .unwrap();
        index(
            &s,
            "out",
            "gmail-thread",
            "email:me@example.com",
            Some("email:sample@example.com"),
            true,
            2_000,
        );
        assert!(ev.owner_replied_since(&m));
    }

    #[test]
    fn a_gmail_send_in_the_thread_counts() {
        let (_d, s) = store();
        let ev = StoreReplyEvidence::new(Arc::clone(&s));
        let m = inbound("thread-9", "email:sample@example.com");
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO outbound_thread_log (entity_id, message_id, thread_id, sent_at_ms, recorded_at_ms) \
                 VALUES ('google:me', 'sent-1', 'thread-9', 2000, 2000)",
                [],
            )
        })
        .unwrap();
        assert!(ev.owner_replied_since(&m));
    }
}
