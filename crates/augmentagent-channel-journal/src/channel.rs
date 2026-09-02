//! ShadowNote → wiki ingest poller.
//!
//! DataStore delta sync (`syncEntries(lastSync)`) with the watermark
//! persisted in `journal_sync_state`; each new/changed entry is decrypted,
//! flattened to text, and handed to the existing `spawn_ingest` pipeline as
//! a `DecisionKind::Capture` / `IngestTrigger::Journal` — the same
//! zero-pipeline-changes shape the voice channel uses.
//!
//! Pass semantics (#900, from the 2026-08-31 OOM post-mortem #897):
//!
//! - **Resumable.** The in-progress pass — the `lastSync` it was issued
//!   with, the server's `startedAt`, and the token of the next unprocessed
//!   page — is persisted in `journal_sync_cursor` after every page. A
//!   restart resumes pagination instead of replaying the whole batch. On
//!   completion the cursor is cleared and `startedAt` becomes the
//!   watermark.
//! - **Idempotent.** Every `(entry id, _version)` handed to ingest is
//!   recorded in `journal_ingested`; a re-served row is skipped. Edits bump
//!   `_version`, so they remain fresh observations.
//! - **Capped.** At most `max_entries_per_poll` entries are handed to
//!   ingest per tick; the remainder is picked up on later ticks through the
//!   cursor. The reasoner fans each entry out to a CLI subprocess, so an
//!   uncapped pass is a fork bomb (297 concurrent `claude -p` on 2026-08-31).
//! - **Base-sync refusal.** With a watermark in hand, a delta poll that
//!   keeps returning rows past `base_sync_threshold` is not a delta — it is
//!   DataStore's silent base-sync fallback (e.g. `lastSync` older than the
//!   delta-sync TTL). Such a pass is refused: nothing is ingested, the
//!   watermark is left alone, and the operator chooses between
//!   `augmentagent journal backfill` (import deliberately, capped) and
//!   `augmentagent journal skip-to-now` (follow new entries only).
//! - **Cannot hang (#901).** Every request has connect/request timeouts
//!   (see `client`), a poll may not outlive half the tick interval, and
//!   pagination stops after `max_pages_per_poll` pages. A hung or looping
//!   upstream therefore surfaces as a logged failure and the next tick
//!   still fires; the persisted cursor resumes whatever was left.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use augmentagent_channel_core::decision::DecisionKind;
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::reasoner::Reasoner;
use augmentagent_store::{Email, JournalSyncCursor, Store};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::client::{Entry, EntryPage, JournalApi, JournalError, ShadowNoteClient};
use crate::config::JournalConfig;
use crate::crypto::{decrypt_entry_content, CryptoError, DekProvider, KmsDekProvider};
use crate::html::html_to_text;

pub struct JournalChannelConfig {
    pub owner_id: String,
    /// Count and log instead of firing ingest; the sync watermark is not
    /// advanced (and no cursor / dedupe row is written) so a later live run
    /// still sees everything.
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub poll_interval: Duration,
    /// #900 — upper bound on entries handed to ingest per `poll_once`.
    pub max_entries_per_poll: usize,
    /// #900 — a delta poll (watermark present) that returns at least this
    /// many rows is treated as a base-sync fallback and refused unless
    /// `allow_base_sync`.
    pub base_sync_threshold: usize,
    /// #900 — operator opt-in for a deliberate full import.
    pub allow_base_sync: bool,
    /// #901 — pages fetched per `poll_once` before it yields with the
    /// cursor persisted; a bound on a server that never stops paginating.
    pub max_pages_per_poll: usize,
}

/// #900 — AppSync DataStore's delta-sync TTL defaults to 30 minutes; polling
/// at exactly that cadence makes every tick a coin-flip between a delta and
/// a silent full-table base sync. Journal writes are low-frequency, so a
/// shorter interval costs nothing and stays well inside the TTL.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// #900 — per-tick ingest budget. ~300/h at the default interval, plenty
/// for a journal, and each one is a CLI subprocess.
pub const DEFAULT_MAX_ENTRIES_PER_POLL: usize = 50;
/// #900 — rows a delta poll may return before it is called a base sync.
pub const DEFAULT_BASE_SYNC_THRESHOLD: usize = 500;
/// #901 — pages per poll. 50 × PAGE_LIMIT(100) rows is far beyond any
/// legitimate delta; the cursor carries the rest to the next tick.
pub const DEFAULT_MAX_PAGES_PER_POLL: usize = 50;

#[derive(Debug, Default)]
pub struct PollOutcome {
    pub pages: usize,
    pub entries_seen: usize,
    pub tombstones: usize,
    pub decrypt_failures: usize,
    /// Entries handed to the ingest step this pass (dry-run: would-be).
    pub processed: usize,
    /// Subset of `processed` that decrypted to text and reached ingest.
    pub ingested: usize,
    /// Rows already recorded in `journal_ingested` at this `_version`.
    pub skipped_already_ingested: usize,
    /// Rows seen but left for a later tick: `max_entries_per_poll` ran out.
    pub deferred: usize,
    /// This pass resumed a persisted in-progress cursor.
    pub resumed: bool,
    /// Refused as a base-sync fallback: nothing ingested, nothing persisted.
    pub refused: bool,
    /// Set only when the pass completed and the watermark advanced.
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
        // #901 — a poll may never outlive half the interval. Dropping the
        // future mid-pass is safe: the cursor is persisted per page (#900),
        // so the next tick resumes rather than replays.
        let deadline = (self.config.poll_interval / 2).max(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("journal channel: shutdown signal received");
                    return Ok(());
                }
                _ = tick.tick() => {
                    match tokio::time::timeout(deadline, self.poll_once()).await {
                        Ok(Ok(outcome)) if outcome.refused => warn!(
                            ?outcome,
                            "journal poll refused (base-sync fallback) — \
                             `augmentagent journal backfill` or `journal skip-to-now`"
                        ),
                        Ok(Ok(outcome)) => info!(?outcome, "journal poll complete"),
                        Ok(Err(e)) => error!("journal poll failed: {e:#}"),
                        Err(_) => error!(
                            deadline_secs = deadline.as_secs_f64(),
                            "journal poll timed out; the persisted cursor resumes next tick"
                        ),
                    }
                }
            }
        }
    }

    async fn fetch_page(
        &self,
        last_sync: Option<i64>,
        token: Option<String>,
        outcome: &mut PollOutcome,
        started_at: &mut Option<i64>,
    ) -> Result<EntryPage, JournalError> {
        let page = self.api.sync_entries(last_sync, token).await?;
        outcome.pages += 1;
        if started_at.is_none() {
            *started_at = page.started_at;
        }
        Ok(page)
    }

    pub async fn poll_once(&self) -> Result<PollOutcome, JournalError> {
        let owner = self.config.owner_id.as_str();
        let dry_run = self.config.dry_run;
        let store_err = |e: augmentagent_store::StoreError| JournalError::Store(e.to_string());
        let mut outcome = PollOutcome::default();

        // Resume an interrupted pass. Dry-run persists nothing, so it never
        // resumes either.
        let cursor = if dry_run {
            None
        } else {
            self.store
                .get_journal_sync_cursor(owner)
                .map_err(store_err)?
        };
        let (query_last_sync, mut next_token, mut started_at) = match &cursor {
            Some(c) => (c.last_sync_ms, c.next_token.clone(), Some(c.started_at_ms)),
            None => (
                self.store
                    .get_journal_sync_state(owner)
                    .map_err(store_err)?,
                None,
                None,
            ),
        };
        outcome.resumed = cursor.is_some();

        let max_pages = self.config.max_pages_per_poll.max(1);

        // Phase A — base-sync detection for a fresh delta pass: buffer pages
        // (no side effects) until the pass ends or the threshold is crossed.
        let mut pending: VecDeque<(Option<String>, EntryPage)> = VecDeque::new();
        if !outcome.resumed && query_last_sync.is_some() && !self.config.allow_base_sync {
            let mut seen = 0usize;
            loop {
                let token_used = next_token.clone();
                let page = self
                    .fetch_page(query_last_sync, token_used.clone(), &mut outcome, &mut started_at)
                    .await?;
                seen += page.items.len();
                next_token = page.next_token.clone();
                pending.push_back((token_used, page));
                if next_token.is_none()
                    || seen >= self.config.base_sync_threshold
                    || outcome.pages >= max_pages
                {
                    break;
                }
            }
            if seen >= self.config.base_sync_threshold {
                warn!(
                    owner_id = %owner,
                    seen,
                    threshold = self.config.base_sync_threshold,
                    last_sync = ?query_last_sync,
                    "journal poll refused: a delta sync returned a full-journal page set \
                     (DataStore base-sync fallback?). Nothing ingested; watermark unchanged. \
                     Run `augmentagent journal backfill` to import deliberately, or \
                     `augmentagent journal skip-to-now` to follow new entries only"
                );
                outcome.entries_seen = seen;
                outcome.refused = true;
                return Ok(outcome);
            }
        }

        // Phase B — process page by page, persisting the cursor as we go.
        let mut budget = self.config.max_entries_per_poll;
        let processed: Result<bool, JournalError> = async {
            loop {
                let (token_used, page) = match pending.pop_front() {
                    Some(p) => p,
                    None => {
                        if outcome.pages >= max_pages {
                            // #901 — the cursor for the next page was
                            // persisted when the previous one completed.
                            return Ok(false);
                        }
                        let token_used = next_token.clone();
                        let page = self
                            .fetch_page(
                                query_last_sync,
                                token_used.clone(),
                                &mut outcome,
                                &mut started_at,
                            )
                            .await?;
                        next_token = page.next_token.clone();
                        (token_used, page)
                    }
                };

                let mut exhausted_mid_page = false;
                for entry in &page.items {
                    outcome.entries_seen += 1;
                    if entry.deleted == Some(true) {
                        // v1 leaves any previously-ingested wiki trace in
                        // place; tombstones are only counted (see #427).
                        outcome.tombstones += 1;
                        continue;
                    }
                    let version = entry.version.unwrap_or(0);
                    if !dry_run
                        && self
                            .store
                            .journal_entry_ingested(owner, &entry.id, version)
                            .map_err(store_err)?
                    {
                        outcome.skipped_already_ingested += 1;
                        continue;
                    }
                    if budget == 0 {
                        outcome.deferred += 1;
                        exhausted_mid_page = true;
                        continue;
                    }
                    budget -= 1;
                    outcome.processed += 1;
                    if !dry_run {
                        self.store
                            .mark_journal_ingested(owner, &entry.id, version)
                            .map_err(store_err)?;
                    }
                    match self.ingest_entry(entry).await {
                        Ok(true) => outcome.ingested += 1,
                        Ok(false) => {}
                        Err(_) => outcome.decrypt_failures += 1,
                    }
                }

                if !exhausted_mid_page && page.next_token.is_none() {
                    return Ok(true);
                }
                // Where the next tick starts: mid-page → refetch this page
                // (dedupe skips the finished rows); otherwise the next page.
                let resume_token = if exhausted_mid_page {
                    token_used
                } else {
                    page.next_token.clone()
                };
                if !dry_run {
                    self.store
                        .set_journal_sync_cursor(
                            owner,
                            &JournalSyncCursor {
                                last_sync_ms: query_last_sync,
                                started_at_ms: started_at.unwrap_or_else(now_ms),
                                next_token: resume_token,
                            },
                        )
                        .map_err(store_err)?;
                }
                if exhausted_mid_page || budget == 0 {
                    // Already-fetched rows we know are waiting, for the log.
                    outcome.deferred += pending.iter().map(|(_, p)| p.items.len()).sum::<usize>();
                    return Ok(false);
                }
            }
        }
        .await;

        let complete = match processed {
            Ok(c) => c,
            Err(e) => {
                if outcome.resumed && !dry_run {
                    // A resumed pass failing again (expired page token, …)
                    // must not wedge the channel: drop the cursor so the next
                    // tick issues a fresh query. `journal_ingested` keeps the
                    // re-walk cheap.
                    warn!(owner_id = %owner, "journal resume failed: {e}; clearing cursor");
                    self.store
                        .clear_journal_sync_cursor(owner)
                        .map_err(store_err)?;
                }
                return Err(e);
            }
        };

        if complete {
            let wm = started_at.unwrap_or_else(now_ms);
            if !dry_run {
                self.store
                    .set_journal_sync_state(owner, wm)
                    .map_err(store_err)?;
                self.store
                    .clear_journal_sync_cursor(owner)
                    .map_err(store_err)?;
            }
            outcome.watermark_ms = Some(wm);
        }
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

    /// Serves canned pages; records every `(lastSync, nextToken)` it was
    /// asked for. `fail_once_at` makes the first fetch of that page index
    /// return an API error (then clears itself) — the "daemon died / server
    /// hiccup mid-pass" simulation for the resumable-cursor tests (#900).
    struct FakeApi {
        pages: Vec<EntryPage>,
        calls: Mutex<Vec<(Option<i64>, Option<String>)>>,
        fail_once_at: Mutex<Option<usize>>,
        /// #901 — when set, every fetch records the call and then never
        /// answers (a request with no timeout against a dead upstream).
        hang: Mutex<bool>,
    }
    #[async_trait]
    impl JournalApi for FakeApi {
        async fn sync_entries(
            &self,
            last_sync: Option<i64>,
            next_token: Option<String>,
        ) -> Result<EntryPage, JournalError> {
            self.calls
                .lock()
                .unwrap()
                .push((last_sync, next_token.clone()));
            if *self.hang.lock().unwrap() {
                std::future::pending::<()>().await;
            }
            let idx = next_token
                .as_deref()
                .and_then(|t| t.parse::<usize>().ok())
                .unwrap_or(0);
            let mut fail = self.fail_once_at.lock().unwrap();
            if *fail == Some(idx) {
                *fail = None;
                return Err(JournalError::Api {
                    message: "simulated failure".into(),
                });
            }
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

    /// A page of content-less live entries `"{prefix}{i}"`; content-less
    /// rows never reach decrypt, which keeps the 600-row fixtures fast.
    fn page(prefix: &str, n: usize, next: Option<&str>, started_at: i64) -> EntryPage {
        EntryPage {
            items: (0..n)
                .map(|i| entry(&format!("{prefix}{i}"), false, None))
                .collect(),
            next_token: next.map(str::to_string),
            started_at: Some(started_at),
        }
    }

    async fn encrypted(html: &str) -> String {
        crate::crypto::encrypt_entry_content(html, "arn:fake", &FixedDek)
            .await
            .unwrap()
    }

    #[derive(Clone, Copy)]
    struct Opts {
        dry_run: bool,
        cap: usize,
        threshold: usize,
        allow_base_sync: bool,
        interval: Duration,
        max_pages: usize,
    }
    const LIVE: Opts = Opts {
        dry_run: false,
        cap: 1_000,
        threshold: 500,
        allow_base_sync: false,
        interval: DEFAULT_POLL_INTERVAL,
        max_pages: DEFAULT_MAX_PAGES_PER_POLL,
    };
    const DRY: Opts = Opts {
        dry_run: true,
        ..LIVE
    };

    /// Store on a tempdir (not a NamedTempFile — WAL sidecars leak, #877).
    fn fresh_store() -> (Arc<Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("journal-test.db")).unwrap());
        (store, dir)
    }

    /// A channel over an *existing* store — the "daemon restarted" shape.
    fn channel_on(
        store: Arc<Store>,
        pages: Vec<EntryPage>,
        opts: Opts,
    ) -> JournalChannel<FakeApi, NoopReasoner> {
        let api = Arc::new(FakeApi {
            pages,
            calls: Mutex::new(Vec::new()),
            fail_once_at: Mutex::new(None),
            hang: Mutex::new(false),
        });
        let config = JournalChannelConfig {
            owner_id: "owner-1".into(),
            dry_run: opts.dry_run,
            wiki_root: None,
            wiki_schema_path: None,
            poll_interval: opts.interval,
            max_entries_per_poll: opts.cap,
            base_sync_threshold: opts.threshold,
            allow_base_sync: opts.allow_base_sync,
            max_pages_per_poll: opts.max_pages,
        };
        JournalChannel::new(store, api, Arc::new(FixedDek), Arc::new(NoopReasoner), config)
    }

    fn channel(
        pages: Vec<EntryPage>,
        opts: Opts,
    ) -> (
        JournalChannel<FakeApi, NoopReasoner>,
        Arc<Store>,
        tempfile::TempDir,
    ) {
        let (store, dir) = fresh_store();
        (channel_on(Arc::clone(&store), pages, opts), store, dir)
    }

    fn calls(ch: &JournalChannel<FakeApi, NoopReasoner>) -> Vec<(Option<i64>, Option<String>)> {
        ch.api.calls.lock().unwrap().clone()
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
        let (ch, _store, _d) = channel(vec![page], DRY);
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
        let (ch, store, _d) = channel(vec![page], DRY);
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
        let (ch, store, _d) = channel(vec![page.clone(), page], LIVE);
        ch.poll_once().await.unwrap();
        assert_eq!(
            store.get_journal_sync_state("owner-1").unwrap(),
            Some(1_751_222_333_444)
        );
        // Second pass must send the persisted watermark as lastSync.
        ch.poll_once().await.unwrap();
        assert_eq!(
            calls(&ch),
            vec![(None, None), (Some(1_751_222_333_444), None)]
        );
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
        let (ch, _store, _d) = channel(vec![p0, p1], DRY);
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
        let (ch, _store, _d) = channel(vec![page], DRY);
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.decrypt_failures, 1);
        assert_eq!(outcome.ingested, 1);
    }

    // ---- #900: resumable cursor, dedupe, per-poll cap, base-sync refusal ----

    #[tokio::test]
    async fn cursor_advances_per_page_and_resumes_after_restart() {
        let pages = vec![
            page("a", 2, Some("1"), 10),
            page("b", 2, Some("2"), 10),
            page("c", 2, None, 10),
        ];
        let (ch, store, _d) = channel(pages.clone(), LIVE);
        *ch.api.fail_once_at.lock().unwrap() = Some(2);

        // Pass 1 dies fetching page 2: pages 0–1 were processed and the
        // cursor must already point at page 2; the watermark is untouched.
        assert!(ch.poll_once().await.is_err());
        let cur = store
            .get_journal_sync_cursor("owner-1")
            .unwrap()
            .expect("in-progress cursor persisted before the failing page");
        assert_eq!(cur.next_token.as_deref(), Some("2"));
        assert_eq!(cur.last_sync_ms, None);
        assert_eq!(cur.started_at_ms, 10);
        assert_eq!(store.get_journal_sync_state("owner-1").unwrap(), None);

        // "Restart": a fresh channel over the same store resumes at page 2
        // with the ORIGINAL lastSync, never re-serving pages 0–1.
        let ch2 = channel_on(Arc::clone(&store), pages, LIVE);
        let out = ch2.poll_once().await.unwrap();
        assert!(out.resumed);
        assert_eq!(calls(&ch2), vec![(None, Some("2".into()))]);
        assert_eq!(out.processed, 2);
        assert_eq!(out.watermark_ms, Some(10));
        assert_eq!(store.get_journal_sync_state("owner-1").unwrap(), Some(10));
        assert!(store.get_journal_sync_cursor("owner-1").unwrap().is_none());
    }

    #[tokio::test]
    async fn already_ingested_versions_are_skipped() {
        let p = page("e", 3, None, 10);
        let (ch, store, _d) = channel(vec![p.clone()], LIVE);
        let first = ch.poll_once().await.unwrap();
        assert_eq!(first.processed, 3);
        assert_eq!(first.skipped_already_ingested, 0);

        // The same (id, _version) rows come back → nothing is re-ingested.
        let ch2 = channel_on(Arc::clone(&store), vec![p.clone()], LIVE);
        let second = ch2.poll_once().await.unwrap();
        assert_eq!(second.processed, 0);
        assert_eq!(second.skipped_already_ingested, 3);

        // An edited entry (bumped _version) is a fresh observation.
        let mut bumped = p;
        bumped.items[0].version = Some(4);
        let ch3 = channel_on(Arc::clone(&store), vec![bumped], LIVE);
        let third = ch3.poll_once().await.unwrap();
        assert_eq!(third.processed, 1);
        assert_eq!(third.skipped_already_ingested, 2);
    }

    #[tokio::test]
    async fn poll_caps_entries_and_defers_remainder() {
        let opts = Opts { cap: 50, ..LIVE };
        let (ch, store, _d) = channel(vec![page("e", 120, None, 10)], opts);

        let p1 = ch.poll_once().await.unwrap();
        assert_eq!(p1.processed, 50);
        assert_eq!(p1.deferred, 70);
        assert_eq!(p1.watermark_ms, None, "pass incomplete → no watermark");
        assert!(store.get_journal_sync_cursor("owner-1").unwrap().is_some());
        assert_eq!(store.get_journal_sync_state("owner-1").unwrap(), None);

        let p2 = ch.poll_once().await.unwrap();
        assert!(p2.resumed);
        assert_eq!(p2.skipped_already_ingested, 50);
        assert_eq!(p2.processed, 50);
        assert_eq!(p2.deferred, 20);

        let p3 = ch.poll_once().await.unwrap();
        assert_eq!(p3.skipped_already_ingested, 100);
        assert_eq!(p3.processed, 20);
        assert_eq!(p3.deferred, 0);
        assert_eq!(p3.watermark_ms, Some(10));
        assert_eq!(store.get_journal_sync_state("owner-1").unwrap(), Some(10));
        assert!(store.get_journal_sync_cursor("owner-1").unwrap().is_none());
    }

    #[tokio::test]
    async fn full_journal_with_a_watermark_is_refused_until_backfill() {
        // 600 rows across 6 full pages — what a DataStore base-sync fallback
        // looks like when the poller *thinks* it is doing a delta.
        let pages: Vec<EntryPage> = (0..6)
            .map(|p| {
                let next = (p < 5).then(|| (p + 1).to_string());
                page(&format!("p{p}-"), 100, next.as_deref(), 10)
            })
            .collect();
        let opts = Opts { cap: 50, ..LIVE };

        let (ch, store, _d) = channel(pages.clone(), opts);
        store.set_journal_sync_state("owner-1", 999).unwrap();
        let out = ch.poll_once().await.unwrap();
        assert!(out.refused);
        assert_eq!(out.processed, 0);
        assert!(out.entries_seen >= 500);
        assert_eq!(store.get_journal_sync_state("owner-1").unwrap(), Some(999));
        assert!(store.get_journal_sync_cursor("owner-1").unwrap().is_none());
        let c = calls(&ch);
        assert!(c.len() <= 5, "stops fetching once the threshold is crossed");
        assert!(c.iter().all(|(ls, _)| *ls == Some(999)));

        // First-ever sync (no watermark) is a legitimate base sync → allowed, capped.
        let (ch2, _s2, _d2) = channel(pages.clone(), opts);
        let out2 = ch2.poll_once().await.unwrap();
        assert!(!out2.refused);
        assert_eq!(out2.processed, 50);

        // Explicit opt-in (`augmentagent journal backfill`) overrides the refusal.
        let ch3 = channel_on(
            Arc::clone(&store),
            pages,
            Opts {
                allow_base_sync: true,
                ..opts
            },
        );
        let out3 = ch3.poll_once().await.unwrap();
        assert!(!out3.refused);
        assert_eq!(out3.processed, 50);
    }

    #[tokio::test]
    async fn dry_run_persists_neither_cursor_nor_dedupe() {
        let opts = Opts { cap: 50, ..DRY };
        let (ch, store, _d) = channel(vec![page("e", 120, None, 10)], opts);
        let p1 = ch.poll_once().await.unwrap();
        assert_eq!(p1.processed, 50);
        assert_eq!(p1.deferred, 70);
        assert!(store.get_journal_sync_cursor("owner-1").unwrap().is_none());
        // A second dry run starts over — nothing was marked ingested.
        let p2 = ch.poll_once().await.unwrap();
        assert!(!p2.resumed);
        assert_eq!(p2.processed, 50);
        assert_eq!(p2.skipped_already_ingested, 0);
    }

    // ---- #901: a hung request must not freeze the channel loop ----

    #[tokio::test]
    async fn hung_page_fetch_does_not_block_the_loop() {
        // 200 ms interval → 100 ms poll deadline. Every fetch hangs; the
        // loop must time each poll out and keep ticking, not park forever
        // inside the first `poll_once` (which is what froze the watermark
        // on 2026-08-31).
        let opts = Opts {
            interval: Duration::from_millis(200),
            ..LIVE
        };
        let (ch, _store, _d) = channel(vec![page("e", 1, None, 10)], opts);
        *ch.api.hang.lock().unwrap() = true;
        let shutdown = CancellationToken::new();
        tokio::select! {
            r = ch.run(shutdown.clone()) => r.unwrap(),
            _ = tokio::time::sleep(Duration::from_millis(700)) => shutdown.cancel(),
        }
        let attempts = calls(&ch).len();
        assert!(attempts >= 2, "loop parked after the first hung poll: {attempts} attempt(s)");
    }

    #[tokio::test]
    async fn pagination_is_bounded_per_poll() {
        // A server that always hands back a next token must not spin the
        // poller forever: stop at `max_pages_per_poll` with the cursor
        // persisted so the next tick continues from there.
        let looping = page("e", 1, Some("0"), 10);
        let opts = Opts { max_pages: 7, ..LIVE };
        let (ch, store, _d) = channel(vec![looping], opts);
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.pages, 7);
        assert_eq!(out.watermark_ms, None, "pass is not complete");
        assert!(store.get_journal_sync_cursor("owner-1").unwrap().is_some());
    }

    #[test]
    fn default_poll_interval_stays_inside_the_delta_sync_ttl() {
        // AppSync DataStore delta-sync TTL defaults to 30 min; polling at
        // exactly that cadence risks a silent base-sync fallback.
        assert!(DEFAULT_POLL_INTERVAL < Duration::from_secs(30 * 60));
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
