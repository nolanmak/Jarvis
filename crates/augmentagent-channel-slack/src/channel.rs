//! `SlackChannel` — polls every active `channel_subscriptions` row with
//! `platform='slack'`, fetches messages since `last_seen_message_id`, and
//! dispatches each message by the subscription's mode.
//!
//! Mirrors the Discord channel's triage pipeline so behavior is consistent
//! across platforms; where Slack differs (API paging via `oldest` instead of
//! snowflakes, subtype filtering instead of message-type ints) the
//! differences are isolated to this module.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{
    ActionStatus, ChannelSubscription, Email, Store, SubscriptionMode, TriageResult,
};
use augmentagent_wiki::IdentityIndex;

use crate::api::{SlackClient, SlackError};
use crate::types::SlackMessage;
use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// Mirror LinkedIn + Discord's 4h cadence; Slack's API has very generous
/// rate limits so this is conservative.
pub const DEFAULT_POLL_SECS: u64 = 4 * 60 * 60;
pub const JITTER_SECS: u64 = 30 * 60;

pub const MAX_MESSAGES_PER_TICK: u32 = 200;

#[derive(Clone, Debug)]
pub struct SlackChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    pub skill_dir: PathBuf,
}

impl Default for SlackChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/slack-triage"),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub subscriptions_polled: usize,
    pub messages_seen: usize,
    pub priority_skipped: usize,
    pub priority_flagged: usize,
    pub priority_replied_dry_run: usize,
    pub priority_awaiting_approval: usize,
    pub digest_stored: usize,
    pub store_only_stored: usize,
    pub errors: usize,
}

pub struct SlackChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub client: Arc<SlackClient>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: SlackChannelConfig,
    pub identity_index: Option<Arc<IdentityIndex>>,
    /// Slack user id of the authenticated account (from SlackAuth) — used to
    /// skip our own outbound messages on ingest.
    pub my_user_id: String,
    wiki_schema: Option<String>,
}

impl<R: Reasoner + 'static> SlackChannel<R> {
    pub fn new(
        store: Arc<Store>,
        client: Arc<SlackClient>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        my_user_id: String,
        config: SlackChannelConfig,
        identity_index: Option<Arc<IdentityIndex>>,
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
            client,
            reasoner,
            approvals,
            config,
            identity_index,
            my_user_id,
            wiki_schema,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("slack channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "slack poll complete"),
                        Err(e) => error!("slack poll failed: {e:#}"),
                    }
                    let jitter = jitter_secs();
                    tokio::time::sleep(Duration::from_secs(jitter)).await;
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let subs = self.store.list_active_subscriptions(PLATFORM)?;
        outcome.subscriptions_polled = subs.len();
        if subs.is_empty() {
            debug!("no active slack subscriptions; nothing to poll");
            return Ok(outcome);
        }
        for sub in subs {
            if let Err(e) = self.poll_subscription(&sub, &mut outcome).await {
                outcome.errors += 1;
                error!(sub_id = %sub.id, channel_id = %sub.channel_id, "slack subscription poll failed: {e:#}");
            }
        }
        Ok(outcome)
    }

    async fn poll_subscription(
        &self,
        sub: &ChannelSubscription,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let messages = match self
            .client
            .fetch_messages(
                &sub.channel_id,
                sub.last_seen_message_id.as_deref(),
                MAX_MESSAGES_PER_TICK,
            )
            .await
        {
            Ok(msgs) => msgs,
            Err(SlackError::Composio(msg)) if msg.contains("invalid_auth") => {
                warn!("slack auth invalid — run `augmentagent slack login`");
                anyhow::bail!("invalid_auth");
            }
            Err(e) => return Err(e.into()),
        };

        // Slack returns newest-first; process oldest→newest so last_seen stays monotonic.
        let mut messages = messages;
        messages.reverse();
        outcome.messages_seen += messages.len();

        let mut newest_seen: Option<String> = None;
        for msg in messages {
            newest_seen = Some(msg.ts.clone());

            if msg.user.as_deref() == Some(self.my_user_id.as_str()) {
                continue;
            }
            if !msg.is_default_user_message() {
                continue;
            }
            if msg.text.is_empty() {
                continue;
            }

            if let Err(e) = self.handle_message(sub, msg, outcome).await {
                outcome.errors += 1;
                error!(sub_id = %sub.id, "handle_message failed: {e:#}");
            }
        }

        if let Some(newest) = newest_seen {
            self.store.update_last_seen_message(&sub.id, &newest)?;
        }
        Ok(())
    }

    async fn handle_message(
        &self,
        sub: &ChannelSubscription,
        msg: SlackMessage,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let email = message_to_email(&msg, sub, &self.my_user_id);
        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            return Ok(());
        }

        match sub.mode {
            SubscriptionMode::Priority => self.handle_priority(email, outcome).await,
            SubscriptionMode::Digest => {
                outcome.digest_stored += 1;
                Ok(())
            }
            SubscriptionMode::StoreOnly => {
                outcome.store_only_stored += 1;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::DigestOnly)?;
                Ok(())
            }
        }
    }

    async fn handle_priority(
        &self,
        email: Email,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let wiki_hint = self.wiki_hint_for_sender(&email);

        let triage = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, &wiki_hint, "");
        let raw = self.reasoner.call(&triage, &triage_prompt).await?;
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
                outcome.priority_skipped += 1;
                Ok(())
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
                outcome.priority_flagged += 1;
                Ok(())
            }
            DecisionKind::Reply => {
                let skill_system =
                    std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                        .unwrap_or_default();
                let draft = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "");
                let drafted = match self.reasoner.call(&draft, &draft_prompt).await {
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
                        Some(&drafted),
                        ActionStatus::DryRun,
                    )?;
                    self.store
                        .mark_email_processed(&email.message_id, TriageResult::Reply)?;
                    println!(
                        "[slack reply dry-run] from={} ({} chars)\n--- draft ---\n{}\n--- /draft ---",
                        email.from,
                        drafted.len(),
                        drafted
                    );
                    self.maybe_ingest(
                        &email,
                        DecisionKind::Reply,
                        decision.reason.as_deref(),
                        Some(&drafted),
                        IngestTrigger::DryRunDrafted,
                    );
                    outcome.priority_replied_dry_run += 1;
                    return Ok(());
                }

                let action_id = self.store.log_action(
                    &email.message_id,
                    email.thread_id.as_deref(),
                    &email.from,
                    &email.subject,
                    Some(&email.body),
                    Some(&drafted),
                    ActionStatus::Pending,
                )?;
                if let Err(e) = self
                    .approvals
                    .post_approval(&action_id, &email, &drafted)
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
                info!(action_id, message_id = %email.message_id, "slack approval card posted");
                outcome.priority_awaiting_approval += 1;
                Ok(())
            }
        }
    }

    fn wiki_hint_for_sender(&self, email: &Email) -> String {
        let Some(index) = &self.identity_index else {
            return String::new();
        };
        let slack_id = extract_slack_id(&email.from).unwrap_or_default();
        if slack_id.is_empty() {
            return String::new();
        }
        match index.lookup(PLATFORM, &slack_id) {
            Some(page) => format!(
                "Sender's wiki page: {} (open with Read; weight the decision by their documented tone/importance).",
                page.slug
            ),
            None => String::new(),
        }
    }

    fn maybe_ingest(
        &self,
        email: &Email,
        decision: DecisionKind,
        reason: Option<&str>,
        draft: Option<&str>,
        trigger: IngestTrigger,
    ) {
        let (Some(root), Some(schema)) = (&self.config.wiki_root, &self.wiki_schema) else {
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

/// Convert a Slack message + its owning subscription into an `Email` row.
pub(crate) fn message_to_email(
    msg: &SlackMessage,
    sub: &ChannelSubscription,
    my_user_id: &str,
) -> Email {
    let author_id = msg.user.clone().unwrap_or_default();
    let author_label = msg
        .username
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| author_id.clone());
    let from = format!("{} <slack:{}>", author_label, author_id);
    let kind = match sub.mode {
        SubscriptionMode::Priority => "dm",
        SubscriptionMode::Digest | SubscriptionMode::StoreOnly => "digest_item",
    };
    Email {
        message_id: format!("{}:{}", sub.channel_id, msg.ts),
        thread_id: Some(sub.channel_id.clone()),
        from,
        subject: String::new(),
        body: msg.text.clone(),
        date: msg.ts.clone(),
        account_entity_id: Some(format!("{ACCOUNT_ENTITY_ID_PREFIX}:{my_user_id}")),
        platform: PLATFORM.to_string(),
        kind: kind.to_string(),
    }
}

/// Parse a Slack user id out of the `from` field shape
/// `"<display> <slack:<user_id>>"`.
fn extract_slack_id(from: &str) -> Option<String> {
    let start = from.rfind("<slack:")? + "<slack:".len();
    let end = from[start..].find('>')?;
    Some(from[start..start + end].to_string())
}

fn jitter_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    ns % (2 * JITTER_SECS + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::ReasonerOpts;

    fn sub_with_mode(mode: SubscriptionMode) -> ChannelSubscription {
        ChannelSubscription {
            id: "sub1".into(),
            platform: PLATFORM.into(),
            channel_id: "C1".into(),
            display_name: "#general".into(),
            mode,
            active: true,
            last_seen_message_id: None,
            last_digest_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn sample_msg(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            message_type: "message".into(),
            subtype: None,
            ts: ts.into(),
            user: Some("U2".into()),
            username: Some("alice".into()),
            text: text.into(),
            thread_ts: None,
            bot_id: None,
        }
    }

    #[test]
    fn message_to_email_priority_uses_dm_kind() {
        let m = sample_msg("100.000001", "hi");
        let s = sub_with_mode(SubscriptionMode::Priority);
        let e = message_to_email(&m, &s, "me");
        assert_eq!(e.platform, "slack");
        assert_eq!(e.kind, "dm");
        assert_eq!(e.thread_id.as_deref(), Some("C1"));
        assert!(e.from.contains("<slack:U2>"));
        assert!(e.from.starts_with("alice"));
    }

    #[test]
    fn message_to_email_digest_uses_digest_item_kind() {
        let m = sample_msg("100.000001", "hi");
        let s = sub_with_mode(SubscriptionMode::Digest);
        let e = message_to_email(&m, &s, "me");
        assert_eq!(e.kind, "digest_item");
    }

    #[test]
    fn extract_slack_id_parses_tag() {
        assert_eq!(extract_slack_id("alice <slack:U123>"), Some("U123".into()));
        assert_eq!(extract_slack_id("no-tag"), None);
    }

    #[test]
    fn jitter_stays_in_window() {
        for _ in 0..50 {
            assert!(jitter_secs() <= 2 * JITTER_SECS);
        }
    }

    struct ScriptedReasoner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl ScriptedReasoner {
        fn new<I: IntoIterator<Item = &'static str>>(r: I) -> Self {
            Self {
                responses: std::sync::Mutex::new(r.into_iter().map(String::from).collect()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _msg: &str) -> anyhow::Result<String> {
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"decision":"skip","reason":"stub"}"#.into()))
        }
    }

    #[derive(Default)]
    struct CountingBroker {
        approvals: std::sync::Mutex<usize>,
        flags: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ApprovalBroker for CountingBroker {
        async fn post_approval(
            &self,
            _id: &str,
            _e: &augmentagent_store::Email,
            _d: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.approvals.lock().unwrap() += 1;
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _e: &augmentagent_store::Email,
            _r: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            *self.flags.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        use rusqlite::Connection;
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL, originalBody TEXT,
                    draftBody TEXT, status TEXT NOT NULL DEFAULT 'pending',
                    errorMessage TEXT, createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT, fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL, body TEXT, receivedAt TEXT,
                    accountEntityId TEXT, firstSeenAt INTEGER NOT NULL,
                    triageResult TEXT, agentProcessedAt INTEGER
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn build_channel<R: Reasoner + 'static>(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
    ) -> SlackChannel<R> {
        let auth = crate::auth::SlackAuth {
            entity_id: "eid".into(),
            connection_id: "cid".into(),
            team_id: "T1".into(),
            team_name: "Test".into(),
            user_id: "me".into(),
            composio_api_key: "ckak_test".into(),
        };
        let client = Arc::new(SlackClient::new(auth).unwrap());
        SlackChannel::new(
            store,
            client,
            reasoner,
            approvals,
            "me".into(),
            SlackChannelConfig {
                dry_run: false,
                poll_interval: Duration::from_secs(1),
                wiki_root: None,
                wiki_schema_path: None,
                skill_dir: PathBuf::from("skills/slack-triage"),
            },
            None,
        )
    }

    #[tokio::test]
    async fn priority_skip_records_skipped() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"newsletter"}"#,
        ]));
        let b: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker::default());
        let ch = build_channel(store.clone(), r, Arc::clone(&b));
        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "deal deal");
        let e = message_to_email(&m, &sub, &ch.my_user_id);
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_skipped, 1);
    }

    #[tokio::test]
    async fn priority_reply_posts_approval() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"actionable"}"#,
            "hey, works for me",
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), r, b);

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "15 min tomorrow?");
        let e = message_to_email(&m, &sub, &ch.my_user_id);
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_awaiting_approval, 1);
        assert_eq!(*counting.approvals.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn priority_flag_posts_flag_notice() {
        let (store, _f) = tmp_store();
        let r = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"unclear"}"#,
        ]));
        let counting = Arc::new(CountingBroker::default());
        let b: Arc<dyn ApprovalBroker> = counting.clone();
        let ch = build_channel(store.clone(), r, b);

        let sub = sub_with_mode(SubscriptionMode::Priority);
        let m = sample_msg("100.000001", "hey");
        let e = message_to_email(&m, &sub, &ch.my_user_id);
        store.upsert_email(&e).unwrap();

        let mut out = PollOutcome::default();
        ch.handle_priority(e, &mut out).await.unwrap();
        assert_eq!(out.priority_flagged, 1);
        assert_eq!(*counting.flags.lock().unwrap(), 1);
    }
}
