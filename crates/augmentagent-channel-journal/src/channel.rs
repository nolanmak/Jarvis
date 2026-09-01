//! ShadowNote → wiki ingest poller.
//!
//! DataStore delta sync (`syncEntries(lastSync)`) with the watermark
//! persisted in `journal_sync_state`; each new/changed entry is decrypted,
//! flattened to text, and handed to the existing `spawn_ingest` pipeline as
//! a `DecisionKind::Capture` / `IngestTrigger::Journal` — the same
//! zero-pipeline-changes shape the voice channel uses.
//!
//! Watermark semantics: the server returns `startedAt` (epoch ms) with the
//! first page; it is persisted only after the *whole* pass completes, so a
//! crash mid-pass re-syncs rather than losing entries. Re-ingest of an
//! already-seen entry is harmless (the ingest prompt updates pages in
//! place) and only happens for rows DataStore reports as changed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use augmentagent_channel_core::decision::DecisionKind;
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::reasoner::Reasoner;
use augmentagent_store::{Email, Store};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::client::{Entry, JournalApi, JournalError, ShadowNoteClient};
use crate::config::JournalConfig;
use crate::crypto::{decrypt_entry_content, CryptoError, DekProvider, KmsDekProvider};
use crate::html::html_to_text;

pub struct JournalChannelConfig {
    pub owner_id: String,
    /// Count and log instead of firing ingest; the sync watermark is not
    /// advanced so a later live run still sees everything.
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub poll_interval: Duration,
}

/// Journal writes are low-frequency; half-hourly is plenty.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Default)]
pub struct PollOutcome {
    pub pages: usize,
    pub entries_seen: usize,
    pub tombstones: usize,
    pub decrypt_failures: usize,
    pub ingested: usize,
    pub watermark_ms: Option<i64>,
}

pub struct JournalChannel<A, R> {
    store: Arc<Store>,
    api: Arc<A>,
    dek: Arc<dyn DekProvider>,
    reasoner: Arc<R>,
    config: JournalChannelConfig,
    /// Loaded `wiki-skill.md`; `None` disables ingest (dry-run still counts).
    wiki_schema: Option<String>,
}

impl<A, R> JournalChannel<A, R>
where
    A: JournalApi,
    R: Reasoner + 'static,
{
    pub fn new(
        store: Arc<Store>,
        api: Arc<A>,
        dek: Arc<dyn DekProvider>,
        reasoner: Arc<R>,
        config: JournalChannelConfig,
    ) -> Self {
        let wiki_schema = match (&config.wiki_root, &config.wiki_schema_path) {
            (Some(_), Some(path)) => match std::fs::read_to_string(path) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn!("journal: failed to read wiki schema {}: {e}", path.display());
                    None
                }
            },
            _ => None,
        };
        Self {
            store,
            api,
            dek,
            reasoner,
            config,
            wiki_schema,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(self.config.poll_interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("journal channel: shutdown signal received");
                    return Ok(());
                }
                _ = tick.tick() => {
                    match self.poll_once().await {
                        Ok(outcome) => info!(?outcome, "journal poll complete"),
                        Err(e) => error!("journal poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    pub async fn poll_once(&self) -> Result<PollOutcome, JournalError> {
        let mut outcome = PollOutcome::default();
        let last_sync = self
            .store
            .get_journal_sync_state(&self.config.owner_id)
            .map_err(|e| JournalError::Store(e.to_string()))?;

        let mut next_token: Option<String> = None;
        let mut watermark: Option<i64> = None;
        loop {
            let page = self.api.sync_entries(last_sync, next_token).await?;
            outcome.pages += 1;
            if watermark.is_none() {
                watermark = page.started_at;
            }
            for entry in &page.items {
                outcome.entries_seen += 1;
                if entry.deleted == Some(true) {
                    // v1 leaves any previously-ingested wiki trace in place;
                    // tombstones are only counted (see #427).
                    outcome.tombstones += 1;
                    continue;
                }
                match self.ingest_entry(entry).await {
                    Ok(true) => outcome.ingested += 1,
                    Ok(false) => {}
                    Err(_) => outcome.decrypt_failures += 1,
                }
            }
            next_token = page.next_token;
            if next_token.is_none() {
                break;
            }
        }

        let wm = watermark.unwrap_or_else(now_ms);
        if !self.config.dry_run {
            self.store
                .set_journal_sync_state(&self.config.owner_id, wm)
                .map_err(|e| JournalError::Store(e.to_string()))?;
        }
        outcome.watermark_ms = Some(wm);
        Ok(outcome)
    }

    /// Ok(true) = counted as ingested (or would-ingest under dry-run).
    /// Errors are decrypt failures — logged by id only, never content.
    async fn ingest_entry(&self, entry: &Entry) -> Result<bool, CryptoError> {
        let Some(content) = entry.content.as_deref() else {
            return Ok(false);
        };
        let html = match decrypt_entry_content(content, self.dek.as_ref()).await {
            Ok(h) => h,
            Err(e) => {
                warn!(entry_id = %entry.id, "journal entry decrypt failed: {e}");
                return Err(e);
            }
        };
        let text = html_to_text(&html);
        if text.is_empty() {
            return Ok(false);
        }
        if self.config.dry_run {
            info!(
                entry_id = %entry.id,
                created_at = %entry.created_at,
                chars = text.len(),
                "dry-run: would ingest journal entry"
            );
            return Ok(true);
        }
        let (Some(root), Some(schema)) = (&self.config.wiki_root, &self.wiki_schema) else {
            return Ok(false);
        };
        spawn_ingest(
            Arc::clone(&self.reasoner),
            root.clone(),
            schema.clone(),
            synthetic_journal_email(entry, &text),
            DecisionKind::Capture,
            Some("shadownote journal sync".to_string()),
            None,
            IngestTrigger::Journal,
        );
        Ok(true)
    }
}

/// Same synthetic-`Email` adaptation the voice channel uses. The
/// `_version` suffix on `message_id` makes an *edited* entry a fresh
/// observation while identical re-deliveries keep a stable id.
pub fn synthetic_journal_email(entry: &Entry, text: &str) -> Email {
    let title = entry
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Journal entry");
    let mut body = String::new();
    if let Some(topic) = entry.topic.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        body.push_str("Topic: ");
        body.push_str(topic);
        body.push_str("\n\n");
    }
    body.push_str(text);
    Email {
        message_id: format!("shadownote:{}:{}", entry.id, entry.version.unwrap_or(0)),
        thread_id: None,
        from: "shadownote".into(),
        subject: format!("Journal: {title}"),
        body,
        date: entry.created_at.clone(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        account_entity_id: Some("shadownote".into()),
        platform: "shadownote".into(),
        kind: "journal_entry".into(),
    }
}

/// Everything the daemon/CLI needs, built from env + keyring. `Ok(None)`
/// = `SHADOWNOTE_*` config absent → feature off; the caller logs and
/// moves on (the daemon must start cleanly without ShadowNote).
pub struct JournalRuntime {
    pub config: JournalConfig,
    pub client: Arc<ShadowNoteClient>,
    pub dek: Arc<KmsDekProvider>,
}

impl JournalRuntime {
    pub async fn from_env() -> Result<Option<Self>, JournalError> {
        let Some(config) = JournalConfig::load() else {
            return Ok(None);
        };
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.region.clone()))
            .load()
            .await;
        let credentials = sdk_config
            .credentials_provider()
            .ok_or_else(|| JournalError::Credentials("no AWS credentials provider".into()))?;
        let client = Arc::new(ShadowNoteClient::new(&config, credentials));
        let dek = Arc::new(KmsDekProvider::new(&sdk_config));
        Ok(Some(Self {
            config,
            client,
            dek,
        }))
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::EntryPage;
    use crate::crypto::GeneratedDek;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct NoopReasoner;
    #[async_trait]
    impl Reasoner for NoopReasoner {
        async fn call(
            &self,
            _opts: &augmentagent_channel_core::reasoner::ReasonerOpts,
            _msg: &str,
        ) -> anyhow::Result<String> {
            Ok("ingested".into())
        }
    }

    struct FixedDek;
    #[async_trait]
    impl DekProvider for FixedDek {
        async fn decrypt_dek(&self, _blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
            Ok(vec![7u8; 32])
        }
        async fn generate_dek(&self, _arn: &str) -> Result<GeneratedDek, CryptoError> {
            Ok(GeneratedDek {
                plaintext: vec![7u8; 32],
                ciphertext_blob: b"blob".to_vec(),
            })
        }
    }

    /// Serves canned pages; records the lastSync values it was asked for.
    struct FakeApi {
        pages: Vec<EntryPage>,
        calls: Mutex<Vec<Option<i64>>>,
    }
    #[async_trait]
    impl JournalApi for FakeApi {
        async fn sync_entries(
            &self,
            last_sync: Option<i64>,
            next_token: Option<String>,
        ) -> Result<EntryPage, JournalError> {
            let mut calls = self.calls.lock().unwrap();
            calls.push(last_sync);
            let idx = next_token
                .as_deref()
                .and_then(|t| t.parse::<usize>().ok())
                .unwrap_or(0);
            Ok(self.pages[idx].clone())
        }
        async fn list_entries(&self, _t: Option<String>) -> Result<EntryPage, JournalError> {
            unimplemented!()
        }
        async fn get_entry(&self, _c: &str) -> Result<Option<Entry>, JournalError> {
            unimplemented!()
        }
        async fn create_entry(&self, _n: crate::client::NewEntry) -> Result<Entry, JournalError> {
            unimplemented!()
        }
    }

    fn entry(id: &str, deleted: bool, content: Option<String>) -> Entry {
        Entry {
            id: id.into(),
            owner_id: "owner-1".into(),
            created_at: "2026-07-01T08:00:00.000Z".into(),
            content,
            title: Some("A day".into()),
            topic: Some("Journal".into()),
            bookmarked: Some(false),
            updated_at: None,
            version: Some(3),
            deleted: Some(deleted),
            last_changed_at: None,
            owner: None,
        }
    }

    async fn encrypted(html: &str) -> String {
        crate::crypto::encrypt_entry_content(html, "arn:fake", &FixedDek)
            .await
            .unwrap()
    }

    fn channel(
        pages: Vec<EntryPage>,
        dry_run: bool,
    ) -> (
        JournalChannel<FakeApi, NoopReasoner>,
        Arc<Store>,
        tempfile::NamedTempFile,
    ) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = Arc::new(Store::open(file.path()).unwrap());
        let api = Arc::new(FakeApi {
            pages,
            calls: Mutex::new(Vec::new()),
        });
        let config = JournalChannelConfig {
            owner_id: "owner-1".into(),
            dry_run,
            wiki_root: None,
            wiki_schema_path: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
        };
        (
            JournalChannel::new(Arc::clone(&store), api, Arc::new(FixedDek), Arc::new(NoopReasoner), config),
            store,
            file,
        )
    }

    #[tokio::test]
    async fn poll_decrypts_counts_and_skips_tombstones() {
        let page = EntryPage {
            items: vec![
                entry("e1", false, Some(encrypted("<p>hello</p>").await)),
                entry("e2", true, Some(encrypted("<p>gone</p>").await)),
                entry("e3", false, None),
            ],
            next_token: None,
            started_at: Some(1_751_000_000_000),
        };
        let (ch, _store, _f) = channel(vec![page], true);
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.entries_seen, 3);
        assert_eq!(outcome.ingested, 1);
        assert_eq!(outcome.tombstones, 1);
        assert_eq!(outcome.decrypt_failures, 0);
        assert_eq!(outcome.watermark_ms, Some(1_751_000_000_000));
    }

    #[tokio::test]
    async fn dry_run_does_not_advance_watermark() {
        let page = EntryPage {
            items: vec![],
            next_token: None,
            started_at: Some(42),
        };
        let (ch, store, _f) = channel(vec![page], true);
        ch.poll_once().await.unwrap();
        assert_eq!(store.get_journal_sync_state("owner-1").unwrap(), None);
    }

    #[tokio::test]
    async fn live_run_persists_watermark_and_reuses_it() {
        let page = EntryPage {
            items: vec![],
            next_token: None,
            started_at: Some(1_751_222_333_444),
        };
        let (ch, store, _f) = channel(vec![page.clone(), page], false);
        ch.poll_once().await.unwrap();
        assert_eq!(
            store.get_journal_sync_state("owner-1").unwrap(),
            Some(1_751_222_333_444)
        );
        // Second pass must send the persisted watermark as lastSync.
        ch.poll_once().await.unwrap();
        let calls = ch.api.calls.lock().unwrap();
        assert_eq!(calls.as_slice(), &[None, Some(1_751_222_333_444)]);
    }

    #[tokio::test]
    async fn pagination_follows_next_token() {
        let p0 = EntryPage {
            items: vec![entry("e1", false, Some(encrypted("<p>a</p>").await))],
            next_token: Some("1".into()),
            started_at: Some(10),
        };
        let p1 = EntryPage {
            items: vec![entry("e2", false, Some(encrypted("<p>b</p>").await))],
            next_token: None,
            started_at: Some(10),
        };
        let (ch, _store, _f) = channel(vec![p0, p1], true);
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.pages, 2);
        assert_eq!(outcome.ingested, 2);
    }

    #[tokio::test]
    async fn decrypt_failure_is_counted_not_fatal() {
        let bad = serde_json::json!({"ciphertext": "AAAA", "ciphertextDEK": "AAAA"}).to_string();
        let page = EntryPage {
            items: vec![
                entry("bad", false, Some(bad)),
                entry("good", false, Some(encrypted("<p>ok</p>").await)),
            ],
            next_token: None,
            started_at: Some(5),
        };
        let (ch, _store, _f) = channel(vec![page], true);
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.decrypt_failures, 1);
        assert_eq!(outcome.ingested, 1);
    }

    #[test]
    fn synthetic_email_shape() {
        let e = entry("abc", false, None);
        let m = synthetic_journal_email(&e, "hello world");
        assert_eq!(m.message_id, "shadownote:abc:3");
        assert_eq!(m.platform, "shadownote");
        assert_eq!(m.kind, "journal_entry");
        assert_eq!(m.subject, "Journal: A day");
        assert!(m.body.starts_with("Topic: Journal\n\n"));
        assert!(m.body.ends_with("hello world"));
    }
}
