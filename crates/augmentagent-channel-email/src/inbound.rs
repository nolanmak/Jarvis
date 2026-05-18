//! `InboundSource` adapter for Gmail (#25).
//!
//! Wraps `GmailApi::fetch_unread` across every active account into the generic
//! [`WorkItem`] shape so Gmail can be driven by
//! [`augmentagent_channel_core::ChannelRunner`] instead of a bespoke poll loop.
//!
//! This intentionally mirrors `augmentagent_channel_telegram_bot`'s
//! `TelegramBotInbound`: the production path stays on
//! [`GmailChannel::poll_once`](crate::GmailChannel) (which owns the rich
//! triage → draft → approve → ingest dispatch and is covered by the existing
//! channel tests); this adapter is the "raw inbox" view downstream code can
//! consume as `WorkItem`s without the reasoner. Behavior of the existing
//! channel is therefore unchanged — this is purely additive.
//!
//! Dedup contract: Gmail's `fetch_unread` only returns messages still carrying
//! the `UNREAD` label, and the channel marks handled mail processed, so
//! successive `fetch_new` calls don't re-yield already-handled items — the
//! same "source owns dedup" guarantee `InboundSource` requires.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{error, warn};

use augmentagent_channel_core::{InboundSource, WorkItem};
use augmentagent_store::Store;

use crate::gmail::GmailApi;

/// Platform discriminator for Gmail rows / work items. Matches
/// `Email::platform`'s default.
pub const PLATFORM: &str = "gmail";

/// Raw-inbox adapter over Gmail. One `fetch_new` call fans out over every
/// active account, exactly like `GmailChannel::poll_once`'s account loop.
pub struct GmailInbound<G: GmailApi> {
    pub store: Arc<Store>,
    pub gmail: Arc<G>,
    /// Per-account fetch cap — same knob as `GmailChannelConfig::per_account_limit`.
    pub per_account_limit: u32,
}

impl<G: GmailApi> GmailInbound<G> {
    pub fn new(store: Arc<Store>, gmail: Arc<G>, per_account_limit: u32) -> Self {
        Self {
            store,
            gmail,
            per_account_limit,
        }
    }
}

/// Build the `WorkItem` for one unread email. `payload` carries the fully
/// serialized `Email` so a handler can reconstruct the typed struct without a
/// re-fetch, exactly as the Telegram adapter embeds the full `Update`.
pub fn email_to_work_item(email: &augmentagent_store::Email) -> WorkItem {
    WorkItem {
        platform: PLATFORM.to_string(),
        kind: if email.kind.is_empty() {
            "dm".to_string()
        } else {
            email.kind.clone()
        },
        external_id: email.message_id.clone(),
        payload: serde_json::to_value(email).unwrap_or(serde_json::Value::Null),
    }
}

#[async_trait]
impl<G: GmailApi + 'static> InboundSource for GmailInbound<G> {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        let accounts = self.store.get_active_gmail_accounts()?;
        if accounts.is_empty() {
            warn!("gmail inbound: no active accounts");
            return Ok(Vec::new());
        }
        let mut items = Vec::new();
        for account in accounts {
            match self
                .gmail
                .fetch_unread(&account.entity_id, self.per_account_limit)
                .await
            {
                Ok(emails) => {
                    for email in &emails {
                        items.push(email_to_work_item(email));
                    }
                }
                Err(e) => {
                    // Mirror poll_once: a single account's fetch failure is
                    // logged and skipped, never aborts the whole tick.
                    error!(account = %account.entity_id, "gmail inbound fetch_unread failed: {e}");
                }
            }
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmail::GmailError;
    use augmentagent_store::Email;
    use std::sync::Mutex;

    struct FakeGmail {
        by_entity: Mutex<std::collections::HashMap<String, Vec<Email>>>,
        fail_entities: Mutex<std::collections::HashSet<String>>,
    }

    impl FakeGmail {
        fn new() -> Self {
            Self {
                by_entity: Mutex::new(std::collections::HashMap::new()),
                fail_entities: Mutex::new(std::collections::HashSet::new()),
            }
        }
    }

    #[async_trait]
    impl GmailApi for FakeGmail {
        async fn fetch_unread(
            &self,
            entity_id: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            if self.fail_entities.lock().unwrap().contains(entity_id) {
                return Err(GmailError::Composio { message: "boom".into() });
            }
            Ok(self
                .by_entity
                .lock()
                .unwrap()
                .get(entity_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn fetch_with_query(
            &self,
            _entity_id: &str,
            _query: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }

        async fn create_draft(
            &self,
            _e: &str,
            _t: &str,
            _s: &str,
            _b: &str,
            _th: Option<&str>,
        ) -> Result<String, GmailError> {
            Ok("draft".into())
        }

        async fn update_draft(
            &self,
            _e: &str,
            _d: &str,
            _t: &str,
            _s: &str,
            _b: &str,
        ) -> Result<(), GmailError> {
            Ok(())
        }

        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), GmailError> {
            Ok(())
        }

        async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), GmailError> {
            Ok(())
        }
    }

    /// Mirror `channel::tests::tmp_store`: real base schema incl.
    /// `gmail_accounts`, seeded with `entities`. `Store::open` layers the
    /// additive migrations on top.
    fn mk_store(entities: &[&str]) -> (Arc<Store>, tempfile::NamedTempFile) {
        use rusqlite::Connection;
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            for (i, ent) in entities.iter().enumerate() {
                conn.execute(
                    "INSERT INTO gmail_accounts VALUES (?1, ?2, ?3, NULL, ?4, 1, 0)",
                    rusqlite::params![
                        format!("a{i}"),
                        format!("c{i}"),
                        format!("{ent}@x.com"),
                        ent
                    ],
                )
                .unwrap();
            }
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn mk_email(id: &str, entity: &str) -> Email {
        Email {
            message_id: id.to_string(),
            thread_id: Some(format!("t-{id}")),
            from: "alice@example.com".to_string(),
            subject: format!("subject {id}"),
            body: format!("body {id}"),
            date: "2026-05-18".to_string(),
            account_entity_id: Some(entity.to_string()),
            platform: "gmail".to_string(),
            kind: "dm".to_string(),
        }
    }

    #[tokio::test]
    async fn fetch_new_fans_out_over_active_accounts() {
        let (store, _f) = mk_store(&["ent-1", "ent-2"]);

        let gmail = Arc::new(FakeGmail::new());
        gmail
            .by_entity
            .lock()
            .unwrap()
            .insert("ent-1".into(), vec![mk_email("m1", "ent-1")]);
        gmail
            .by_entity
            .lock()
            .unwrap()
            .insert("ent-2".into(), vec![mk_email("m2", "ent-2"), mk_email("m3", "ent-2")]);

        let inbound = GmailInbound::new(Arc::clone(&store), gmail, 50);
        let items = inbound.fetch_new().await.unwrap();
        let mut ids: Vec<_> = items.iter().map(|w| w.external_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["m1", "m2", "m3"]);
        assert!(items.iter().all(|w| w.platform == "gmail" && w.kind == "dm"));
        // Payload round-trips back into a typed Email.
        let back: Email = serde_json::from_value(items[0].payload.clone()).unwrap();
        assert_eq!(back.message_id, items[0].external_id);
    }

    #[tokio::test]
    async fn fetch_new_skips_a_failing_account_but_keeps_the_rest() {
        let (store, _f) = mk_store(&["ent-1", "ent-2"]);

        let gmail = Arc::new(FakeGmail::new());
        gmail.fail_entities.lock().unwrap().insert("ent-1".into());
        gmail
            .by_entity
            .lock()
            .unwrap()
            .insert("ent-2".into(), vec![mk_email("ok", "ent-2")]);

        let inbound = GmailInbound::new(Arc::clone(&store), gmail, 50);
        let items = inbound.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "ok");
    }

    #[tokio::test]
    async fn fetch_new_empty_when_no_accounts() {
        let (store, _f) = mk_store(&[]);
        let gmail = Arc::new(FakeGmail::new());
        let inbound = GmailInbound::new(store, gmail, 50);
        assert!(inbound.fetch_new().await.unwrap().is_empty());
    }
}
