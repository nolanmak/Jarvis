//! `InstagramChannel` — polls the DM inbox on a ≥30min ±10min cadence (#18),
//! runs each new text DM through the shared triage → draft → ingest pipeline,
//! and hands drafts to the Discord approval broker. Media-only DMs route to a
//! Discord flag card (not the reasoner — there's no text to triage).
//!
//! `InstagramDmTrigger` is the [`InboundSource`] adapter so the inbox can be
//! consumed as a `Trigger` (`InboundMessageTrigger`) by future generic
//! drivers; the production path is `poll_once`, mirroring the LinkedIn and
//! Telegram channels.
//!
//! Rate-limit safety (#18): on HTTP 429 / a `feedback_required` (suspicious)
//! body the channel pauses itself for 1h, records a governor halt, and logs
//! loudly. The daemon never crashes on a rate-limit — it's expected
//! steady-state, returned as a clean `PollOutcome`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{InboundSource, WorkItem};
use augmentagent_channel_core::{HaltReason, Platform, RateGovernor, Reasoner};
use augmentagent_store::{ActionStatus, Store, TriageResult, NUDGE_INTERVAL_MS};

use crate::api::{InstagramApi, InstagramError};
use crate::failure::FailureKind;
use crate::types::{Dm, PLATFORM};

/// Default poll interval: 30 min (#18). Floor — the jitter only adds.
pub const DEFAULT_POLL_SECS: u64 = 30 * 60;

/// Jitter window: ±10 min around the base interval (#18).
pub const JITTER_SECS: u64 = 10 * 60;

/// Self-pause window on a soft-block / 429 (#18): 1 hour.
pub const SOFT_BLOCK_PAUSE_MS: i64 = 3600 * 1000;

#[derive(Clone, Debug)]
pub struct InstagramChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub skill_dir: PathBuf,
}

impl Default for InstagramChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/instagram-triage"),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub dms_checked: usize,
    pub skipped: usize,
    pub flagged: usize,
    pub media_flagged: usize,
    pub replied_dry_run: usize,
    pub awaiting_approval: usize,
    pub paused: bool,
    pub errors: usize,
}

pub struct InstagramChannel<A: InstagramApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub api: Arc<A>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub governor: Arc<dyn RateGovernor>,
    pub config: InstagramChannelConfig,
    /// The user's own numeric id — filters outbound + carries the
    /// `instagram:<id>` account prefix.
    pub ds_user_id: String,
    wiki_schema: Option<String>,
}

impl<A: InstagramApi, R: Reasoner + 'static> InstagramChannel<A, R> {
    pub fn new(
        store: Arc<Store>,
        api: Arc<A>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        governor: Arc<dyn RateGovernor>,
        ds_user_id: String,
        config: InstagramChannelConfig,
    ) -> Self {
        let wiki_schema = match (&config.wiki_root, &config.wiki_schema_path) {
            (Some(root), Some(schema_path)) => {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                match layout.bootstrap() {
                    Ok(()) => std::fs::read_to_string(schema_path)
                        .ok()
                        .filter(|s| !s.trim().is_empty()),
                    Err(e) => {
                        warn!("wiki bootstrap failed, disabling wiki: {e}");
                        None
                    }
                }
            }
            _ => None,
        };
        Self {
            store,
            api,
            reasoner,
            approvals,
            governor,
            config,
            ds_user_id,
            wiki_schema,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("instagram channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "instagram poll complete"),
                        // poll_once already maps rate-limits to a clean
                        // outcome; a hard Err here is a real bug, logged but
                        // NOT propagated so the daemon survives.
                        Err(e) => error!("instagram poll failed (non-fatal): {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();

        // Respect an existing circuit-breaker halt without burning a fetch.
        if let Some(until) = self.governor.is_halted(Platform::Instagram).await {
            warn!(
                until_ms = until,
                "instagram channel halted (governor); skipping poll"
            );
            outcome.paused = true;
            return Ok(outcome);
        }

        let (dms, _cursor) = match self.api.fetch_inbox(None).await {
            Ok(v) => v,
            Err(e) if e.is_soft_block() => {
                self.pause_on_soft_block(&e).await;
                outcome.paused = true;
                return Ok(outcome);
            }
            Err(e) if e.is_challenge() => {
                self.halt_on_challenge(&e).await;
                outcome.paused = true;
                return Ok(outcome);
            }
            Err(e) => {
                error!("instagram fetch_inbox failed: {e:#}");
                outcome.errors += 1;
                return Ok(outcome);
            }
        };
        outcome.dms_checked = dms.len();

        for dm in dms {
            if dm.is_outbound(&self.ds_user_id) {
                continue;
            }
            match self.handle_dm(dm).await {
                Ok(Some(Dispatch::Skipped)) => outcome.skipped += 1,
                Ok(Some(Dispatch::Flagged)) => outcome.flagged += 1,
                Ok(Some(Dispatch::MediaFlagged)) => outcome.media_flagged += 1,
                Ok(Some(Dispatch::DryRun)) => outcome.replied_dry_run += 1,
                Ok(Some(Dispatch::AwaitingApproval)) => {
                    outcome.awaiting_approval += 1
                }
                Ok(None) => {}
                Err(e) => {
                    outcome.errors += 1;
                    error!("instagram handle_dm failed: {e:#}");
                }
            }
        }
        Ok(outcome)
    }

    /// 1h self-pause + governor halt + loud log on a soft-block / 429 (#18).
    async fn pause_on_soft_block(&self, e: &InstagramError) {
        let until = chrono::Utc::now().timestamp_millis() + SOFT_BLOCK_PAUSE_MS;
        if let Err(err) = self
            .governor
            .record_halt(Platform::Instagram, HaltReason::RateLimitToast, until)
            .await
        {
            error!("failed to persist instagram soft-block halt: {err:#}");
        }
        error!(
            error = %e,
            pause_until_ms = until,
            "INSTAGRAM SOFT-BLOCK / 429 — channel self-paused 1h (data.db rate_halts). \
             This is expected steady-state; daemon continues."
        );
    }

    /// Longer halt + loud alert on a checkpoint/challenge (account flagged).
    async fn halt_on_challenge(&self, e: &InstagramError) {
        let until = chrono::Utc::now().timestamp_millis()
            + FailureKind::Challenge.pause_ms();
        if let Err(err) = self
            .governor
            .record_halt(Platform::Instagram, HaltReason::LoginChallenge, until)
            .await
        {
            error!("failed to persist instagram challenge halt: {err:#}");
        }
        error!(
            error = %e,
            "INSTAGRAM ACCOUNT CHALLENGED — channel halted. Clear the checkpoint in the \
             Instagram app, then re-run `augmentagent instagram login`."
        );
    }

    async fn handle_dm(&self, dm: Dm) -> anyhow::Result<Option<Dispatch>> {
        // Media-only DM → Discord flag card, NOT triage (#18). There's no
        // text for the reasoner; the user opens Instagram if they care.
        if dm.media_only {
            let email = dm.into_email(&self.ds_user_id);
            self.store.upsert_email(&email)?;
            if self.store.is_message_processed(&email.message_id)? {
                return Ok(None);
            }
            self.store.log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                None,
                ActionStatus::Flagged,
            )?;
            self.store
                .mark_email_processed(&email.message_id, TriageResult::Flag)?;
            if let Err(e) = self
                .approvals
                .post_flag_notice(
                    &email,
                    "media-only Instagram DM (photo/clip/voice) — open IG to view",
                )
                .await
            {
                warn!(message_id = %email.message_id, "post_flag_notice failed: {e}");
            }
            return Ok(Some(Dispatch::MediaFlagged));
        }

        let email = dm.into_email(&self.ds_user_id);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(None);
        }
        // Gate re-triage on action presence so the 30min cadence doesn't
        // stack duplicate cards (same fix as the LinkedIn channel).
        if self.store.is_message_processed(&email.message_id)? {
            return Ok(None);
        }

        // --- TRIAGE ---
        let triage_opts = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
        let raw = self.reasoner.call(&triage_opts, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "triage parse failed: {e}; raw={raw}");
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Error,
                )?;
                return Err(e.into());
            }
        };

        match decision.decision {
            DecisionKind::Skip => {
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Skipped,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                self.maybe_ingest(
                    &email,
                    DecisionKind::Skip,
                    decision.reason.as_deref(),
                    None,
                    IngestTrigger::Triaged,
                );
                Ok(Some(Dispatch::Skipped))
            }
            DecisionKind::Flag => {
                self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    None,
                    ActionStatus::Flagged,
                )?;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Flag)?;
                let reason = decision.reason.as_deref().unwrap_or("flagged");
                if let Err(e) = self.approvals.post_flag_notice(&email, reason).await {
                    warn!(message_id = %email.message_id, "post_flag_notice failed: {e}");
                }
                self.maybe_ingest(
                    &email,
                    DecisionKind::Flag,
                    decision.reason.as_deref(),
                    None,
                    IngestTrigger::Triaged,
                );
                Ok(Some(Dispatch::Flagged))
            }
            DecisionKind::Reply => {
                let skill_system =
                    std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                        .unwrap_or_default();
                let draft_opts =
                    draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "", "", "", "", "");
                let draft = match self.reasoner.call(&draft_opts, &draft_prompt).await
                {
                    Ok(s) => s.trim().to_string(),
                    Err(e) => {
                        error!(message_id = %email.message_id, "draft call failed: {e}");
                        self.store.log_action(
                            &email.message_id,
                            email.thread_id.as_deref(),
                            &email.from,
                            &email.subject,
                            Some(&email.body),
                            None,
                            ActionStatus::Error,
                        )?;
                        return Err(e);
                    }
                };

                if self.config.dry_run {
                    self.store.log_action(
                        &email.message_id,
                        email.thread_id.as_deref(),
                        &email.from,
                        &email.subject,
                        Some(&email.body),
                        Some(&draft),
                        ActionStatus::DryRun,
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    println!(
                        "[instagram reply dry-run] {}\n--- draft ---\n{}\n--- /draft ---",
                        email.subject, draft
                    );
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        decision.reason.as_deref(),
                        Some(&draft),
                        IngestTrigger::DryRunDrafted,
                    );
                    return Ok(Some(Dispatch::DryRun));
                }

                let action_id = self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&draft),
                    ActionStatus::Pending,
                )?;
                if let Err(e) = self
                    .approvals
                    .post_approval(&action_id, &email, &draft)
                    .await
                {
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Error,
                        None,
                        Some(&format!("post_approval: {e}")),
                    )?;
                    return Err(anyhow::anyhow!("post_approval: {e}"));
                }
                if let Err(e) = self
                    .store
                    .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
                {
                    warn!(action_id, "record_nudge after post_approval failed: {e}");
                }
                info!(action_id, message_id = %email.message_id, "instagram approval card posted");
                Ok(Some(Dispatch::AwaitingApproval))
            }
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "instagram triage returned non-message decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                Ok(Some(Dispatch::Skipped))
            }
        }
    }

    fn maybe_ingest(
        &self,
        email: &augmentagent_store::Email,
        decision: DecisionKind,
        reason: Option<&str>,
        draft: Option<&str>,
        trigger: IngestTrigger,
    ) {
        let (Some(root), Some(schema)) = (&self.config.wiki_root, &self.wiki_schema)
        else {
            return;
        };
        spawn_ingest(
            Arc::clone(&self.reasoner),
            root.clone(),
            schema.clone(),
            email.clone(),
            decision,
            reason.map(str::to_string),
            draft.map(str::to_string),
            trigger,
        );
    }
}

/// `InboundSource` adapter — wrap the inbox so it can be consumed as a
/// `Trigger` via `InboundMessageTrigger`. The production path is
/// `InstagramChannel::poll_once`; this is the raw-inbox view for future
/// generic drivers (matches the Telegram channel's `TelegramBotInbound`).
pub struct InstagramDmTrigger<A: InstagramApi> {
    pub api: Arc<A>,
    pub ds_user_id: String,
}

#[async_trait]
impl<A: InstagramApi + 'static> InboundSource for InstagramDmTrigger<A> {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        let (dms, _cursor) = match self.api.fetch_inbox(None).await {
            Ok(v) => v,
            // A soft-block is NOT an error for the trigger contract — yield
            // empty so the daemon doesn't crash on rate-limit.
            Err(e) if e.is_soft_block() => {
                warn!(error = %e, "instagram trigger soft-blocked; yielding empty");
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };
        let items = dms
            .into_iter()
            .filter(|d| !d.is_outbound(&self.ds_user_id))
            .map(|d| WorkItem {
                platform: PLATFORM.to_string(),
                kind: "dm".to_string(),
                external_id: d.item_id.clone(),
                payload: serde_json::json!({
                    "thread_id": d.thread_id,
                    "peer_name": d.peer_name,
                    "peer_pk": d.peer_pk,
                    "text": d.text,
                    "media_only": d.media_only,
                }),
            })
            .collect();
        Ok(items)
    }
}

fn jitter_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    ns % (2 * JITTER_SECS + 1)
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    Skipped,
    Flagged,
    MediaFlagged,
    DryRun,
    AwaitingApproval,
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::{
        ReasonerOpts, SqliteGovernor, SystemClock,
    };
    use augmentagent_store::Email;

    struct StubApi {
        dms: Vec<Dm>,
        soft_block: bool,
        challenge: bool,
    }
    #[async_trait]
    impl InstagramApi for StubApi {
        async fn fetch_inbox(
            &self,
            _cursor: Option<&str>,
        ) -> Result<(Vec<Dm>, Option<String>), InstagramError> {
            if self.soft_block {
                return Err(InstagramError::RateLimited(FailureKind::RateLimit));
            }
            if self.challenge {
                return Err(InstagramError::Challenged(FailureKind::Challenge));
            }
            Ok((self.dms.clone(), None))
        }
        async fn send_dm(
            &self,
            _t: &str,
            _x: &str,
        ) -> Result<String, InstagramError> {
            Ok("item-stub".into())
        }
        async fn fetch_user_feed(
            &self,
            _u: &str,
            _c: Option<&str>,
        ) -> Result<(Vec<crate::types::FeedPost>, Option<String>), InstagramError>
        {
            Ok((vec![], None))
        }
        async fn post_comment(
            &self,
            _m: &str,
            _t: &str,
        ) -> Result<String, InstagramError> {
            Ok("cmt-stub".into())
        }
    }

    struct ScriptedReasoner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(r: I) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    r.into_iter().map(String::from).collect(),
                ),
            }
        }
    }
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(
            &self,
            _opts: &ReasonerOpts,
            _u: &str,
        ) -> anyhow::Result<String> {
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    r#"{"decision":"skip","reason":"stub"}"#.into()
                }))
        }
    }

    #[derive(Default)]
    struct RecordingBroker {
        posts: std::sync::Mutex<Vec<String>>,
        flags: std::sync::Mutex<Vec<(String, String)>>,
    }
    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(
            &self,
            id: &str,
            _e: &Email,
            _d: &str,
        ) -> Result<(), ApprovalError> {
            self.posts.lock().unwrap().push(id.to_string());
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            e: &Email,
            r: &str,
        ) -> Result<(), ApprovalError> {
            self.flags
                .lock()
                .unwrap()
                .push((e.message_id.clone(), r.to_string()));
            Ok(())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(file.path()).unwrap();
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
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT,
                    agentProcessedAt INTEGER,
                    platform TEXT NOT NULL DEFAULT 'gmail',
                    kind TEXT NOT NULL DEFAULT 'dm'
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                CREATE TABLE channel_subscriptions (
                    id TEXT PRIMARY KEY, platform TEXT NOT NULL,
                    channel_id TEXT NOT NULL, display_name TEXT NOT NULL,
                    mode TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
                    last_seen_message_id TEXT, last_digest_at_ms INTEGER,
                    created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE slack_workspaces (
                    id TEXT PRIMARY KEY, team_id TEXT NOT NULL UNIQUE,
                    team_name TEXT NOT NULL, entity_id TEXT NOT NULL,
                    connection_id TEXT NOT NULL, user_id TEXT NOT NULL,
                    active INTEGER NOT NULL DEFAULT 1, created_at_ms INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn governor(store: Arc<Store>) -> Arc<dyn RateGovernor> {
        Arc::new(SqliteGovernor::new(store, Arc::new(SystemClock)))
    }

    fn text_dm(id: &str) -> Dm {
        Dm {
            item_id: id.into(),
            thread_id: format!("t-{id}"),
            peer_name: "Tony Siu".into(),
            peer_pk: "123".into(),
            sender_pk: "123".into(),
            text: "hey, got a minute?".into(),
            timestamp_ms: 1_715_900_000_000,
            media_only: false,
        }
    }

    fn build<R: Reasoner + 'static>(
        store: Arc<Store>,
        api: Arc<StubApi>,
        reasoner: Arc<R>,
        broker: Arc<RecordingBroker>,
    ) -> InstagramChannel<StubApi, R> {
        let gov = governor(store.clone());
        InstagramChannel::new(
            store,
            api,
            reasoner,
            broker,
            gov,
            "456".into(),
            InstagramChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn reply_posts_approval_card() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            dms: vec![text_dm("m-reply")],
            soft_block: false,
            challenge: false,
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"direct question"}"#,
            "Sure — Thursday 3pm works.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = build(store.clone(), api, reasoner, broker.clone());
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.awaiting_approval, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(!store.is_email_complete("m-reply").unwrap());
    }

    #[tokio::test]
    async fn media_only_dm_routes_to_flag_card_not_triage() {
        let (store, _f) = tmp_store();
        let mut dm = text_dm("m-media");
        dm.media_only = true;
        dm.text = String::new();
        let api = Arc::new(StubApi {
            dms: vec![dm],
            soft_block: false,
            challenge: false,
        });
        // Empty reasoner queue — must NOT be consulted for media-only.
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = build(store, api, reasoner.clone(), broker.clone());
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.media_flagged, 1);
        assert_eq!(out.awaiting_approval, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        let flags = broker.flags.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert!(flags[0].1.contains("media-only"));
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn soft_block_pauses_channel_without_crashing() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            dms: vec![],
            soft_block: true,
            challenge: false,
        });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = build(store.clone(), api, reasoner, broker);
        let out = ch.poll_once().await.unwrap();
        assert!(out.paused);
        assert_eq!(out.errors, 0);
        // Governor halt persisted ⇒ a subsequent poll short-circuits.
        let out2 = ch.poll_once().await.unwrap();
        assert!(out2.paused);
        assert_eq!(out2.dms_checked, 0);
    }

    #[tokio::test]
    async fn challenge_halts_channel() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            dms: vec![],
            soft_block: false,
            challenge: true,
        });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = build(store, api, reasoner, broker);
        let out = ch.poll_once().await.unwrap();
        assert!(out.paused);
        assert_eq!(out.errors, 0);
    }

    #[tokio::test]
    async fn outbound_messages_are_skipped() {
        let (store, _f) = tmp_store();
        let mut dm = text_dm("m-out");
        dm.sender_pk = "456".into(); // == ds_user_id
        let api = Arc::new(StubApi {
            dms: vec![dm],
            soft_block: false,
            challenge: false,
        });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = build(store, api, reasoner.clone(), broker);
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.dms_checked, 1);
        assert_eq!(out.awaiting_approval, 0);
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dm_with_prior_action_is_not_retriaged() {
        let (store, _f) = tmp_store();
        let dm = text_dm("m-dupe");
        let email = dm.clone().into_email("456");
        store.upsert_email(&email).unwrap();
        store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("prev draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        let api = Arc::new(StubApi {
            dms: vec![dm],
            soft_block: false,
            challenge: false,
        });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = build(store, api, reasoner.clone(), broker.clone());
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.dms_checked, 1);
        assert_eq!(out.awaiting_approval, 0);
        assert_eq!(broker.posts.lock().unwrap().len(), 0);
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn trigger_yields_workitems_excluding_outbound() {
        let mut out_dm = text_dm("m-out");
        out_dm.sender_pk = "456".into();
        let api = Arc::new(StubApi {
            dms: vec![text_dm("m1"), out_dm],
            soft_block: false,
            challenge: false,
        });
        let trig = InstagramDmTrigger {
            api,
            ds_user_id: "456".into(),
        };
        let items = trig.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "m1");
        assert_eq!(items[0].platform, "instagram");
        assert_eq!(items[0].kind, "dm");
    }

    #[tokio::test]
    async fn trigger_soft_block_yields_empty_not_error() {
        let api = Arc::new(StubApi {
            dms: vec![],
            soft_block: true,
            challenge: false,
        });
        let trig = InstagramDmTrigger {
            api,
            ds_user_id: "456".into(),
        };
        let items = trig.fetch_new().await.unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn jitter_stays_in_window() {
        for _ in 0..50 {
            assert!(jitter_secs() <= 2 * JITTER_SECS);
        }
    }
}
