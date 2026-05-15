//! `GithubChannel` — polls the authenticated user's notifications every ~2min
//! (GitHub rate-limits authenticated requests at 5000/h, so this is pocket
//! change), filters to the allowed reason set, hydrates the linked subject,
//! and dispatches each surviving notification by the matching
//! `channel_subscriptions` row's mode.
//!
//! `channel_id` for the GitHub platform is the lowercased `<owner>/<repo>`
//! slug. A notification with no matching subscription is treated as
//! `priority` by default so newly-watched repos start surfacing immediately
//! — opt out via an explicit `store_only` subscribe.

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
    NUDGE_INTERVAL_MS,
};

use crate::api::{GithubApi, GithubError};
use crate::types::{Notification, SubjectDetail, ThreadLocator, TriageCandidate};
use crate::PLATFORM;

/// Default poll interval: 2 minutes. GitHub authenticated quota is 5000
/// requests/h — at 1 list call + ~5 subject hydrations per tick that's well
/// inside the budget.
pub const DEFAULT_POLL_SECS: u64 = 2 * 60;

#[derive(Clone, Debug)]
pub struct GithubChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub wiki_root: Option<PathBuf>,
    pub wiki_schema_path: Option<PathBuf>,
    /// Skill dir for the email-triage crate's learned rubric — reused as-is
    /// since GitHub mentions/review-requests/assignments are "messages we
    /// might respond to" the rubric applies to.
    pub skill_dir: PathBuf,
    /// `true` ⇒ PATCH the notification thread to `read` after dispatch
    /// (Approve / Skip / Flag). When `false` we leave threads unread and let
    /// the user clear them in the GitHub UI. Defaults to `true` so the
    /// daemon doesn't double-surface the same notification on the next tick.
    pub mark_read_on_dispatch: bool,
    /// What to do for a notification whose `<owner>/<repo>` has no row in
    /// `channel_subscriptions`. Defaulting to `Priority` matches how email
    /// works (every inbound is triaged) and keeps the daemon useful out of
    /// the box.
    pub default_mode: SubscriptionMode,
}

impl Default for GithubChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            wiki_root: None,
            wiki_schema_path: None,
            skill_dir: PathBuf::from("skills/email-triage"),
            mark_read_on_dispatch: true,
            default_mode: SubscriptionMode::Priority,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub notifications_seen: usize,
    pub filtered_out: usize,
    pub priority_skipped: usize,
    pub priority_flagged: usize,
    pub priority_replied_dry_run: usize,
    pub priority_awaiting_approval: usize,
    pub digest_stored: usize,
    pub store_only_stored: usize,
    pub already_processed: usize,
    pub errors: usize,
}

pub struct GithubChannel<G: GithubApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub api: Arc<G>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub config: GithubChannelConfig,
    /// Authenticated GitHub login. Stamped on `Email::account_entity_id` so
    /// the approver can route Approve clicks back to the correct PAT.
    pub my_login: String,
    wiki_schema: Option<String>,
}

impl<G: GithubApi, R: Reasoner + 'static> GithubChannel<G, R> {
    pub fn new(
        store: Arc<Store>,
        api: Arc<G>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        my_login: String,
        config: GithubChannelConfig,
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
            config,
            my_login,
            wiki_schema,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("github channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "github poll complete"),
                        Err(e) => error!("github poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let notifs = match self.api.list_notifications(None, false).await {
            Ok(n) => n,
            Err(GithubError::AuthInvalid) => {
                warn!(
                    "github auth invalid — rotate PAT via `augmentagent github login`"
                );
                outcome.errors += 1;
                return Ok(outcome);
            }
            Err(GithubError::RateLimited { reset }) => {
                warn!(reset = ?reset, "github rate limited; backing off");
                outcome.errors += 1;
                return Ok(outcome);
            }
            Err(e) => {
                error!("github list_notifications failed: {e:#}");
                outcome.errors += 1;
                return Ok(outcome);
            }
        };
        outcome.notifications_seen = notifs.len();
        let subs = self
            .store
            .list_active_subscriptions(PLATFORM)
            .unwrap_or_default();

        for notif in notifs {
            let Some(kind) = notif.triage_kind() else {
                outcome.filtered_out += 1;
                continue;
            };
            if let Err(e) = self.handle_notification(notif, kind, &subs, &mut outcome).await {
                outcome.errors += 1;
                error!("handle_notification failed: {e:#}");
            }
        }
        Ok(outcome)
    }

    async fn handle_notification(
        &self,
        notif: Notification,
        kind: &'static str,
        subs: &[ChannelSubscription],
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        let Some(thread_id_u64) = notif.thread_id_u64() else {
            warn!(thread = %notif.id, "non-numeric thread id; skipping");
            outcome.filtered_out += 1;
            return Ok(());
        };
        let mode = resolve_mode(
            &notif.repository.full_name,
            subs,
            self.config.default_mode,
        );

        // Hydrate body (best-effort: missing body just means a skeletal email).
        let detail = match self.api.fetch_subject(&notif.subject.url).await {
            Ok(d) => d,
            Err(e) => {
                warn!(thread = %notif.id, "fetch_subject failed: {e:#}");
                None
            }
        };
        let candidate = build_candidate(notif, kind, detail);
        let email = candidate.into_email(&self.my_login);

        self.store.upsert_email(&email)?;
        if self.store.is_email_complete(&email.message_id)? {
            outcome.already_processed += 1;
            return Ok(());
        }
        if self.store.is_message_processed(&email.message_id)? {
            outcome.already_processed += 1;
            return Ok(());
        }

        match mode {
            SubscriptionMode::StoreOnly => {
                outcome.store_only_stored += 1;
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::DigestOnly)?;
                self.maybe_mark_read(thread_id_u64).await;
                Ok(())
            }
            SubscriptionMode::Digest => {
                // Persist; the cross-channel digest scheduler picks these up
                // by `platform='github'`. Don't mark email complete so the
                // digest worker still sees it.
                outcome.digest_stored += 1;
                Ok(())
            }
            SubscriptionMode::Priority => {
                self.handle_priority(email, thread_id_u64, outcome).await
            }
        }
    }

    async fn handle_priority(
        &self,
        email: Email,
        thread_id_u64: u64,
        outcome: &mut PollOutcome,
    ) -> anyhow::Result<()> {
        // --- TRIAGE (Opus with optional wiki read) ---
        let triage = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
        let raw = self.reasoner.call(&triage, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "github triage parse failed: {e}; raw={raw}");
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
                self.maybe_mark_read(thread_id_u64).await;
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
                let draft_o = draft_opts(skill_system, self.config.wiki_root.clone());
                let draft_prompt = draft_user_message(&email, "", "");
                let drafted = match self.reasoner.call(&draft_o, &draft_prompt).await {
                    Ok(s) => s.trim().to_string(),
                    Err(e) => {
                        error!(message_id = %email.message_id, "github draft call failed: {e}");
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
                        "[github reply dry-run] {}\n--- draft ---\n{}\n--- /draft ---",
                        email.subject, drafted
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
                if let Err(e) = self
                    .store
                    .record_nudge(&action_id, now_millis() + NUDGE_INTERVAL_MS)
                {
                    warn!(action_id, "record_nudge after post_approval failed: {e}");
                }
                info!(action_id, message_id = %email.message_id, "github approval card posted");
                outcome.priority_awaiting_approval += 1;
                Ok(())
            }
            // Capture / Meeting are wave-A wiki-ingest-only kinds emitted by
            // the voice and gcal channels respectively — github triage must
            // never produce them. Defensive skip.
            DecisionKind::Capture | DecisionKind::Meeting => {
                warn!(
                    message_id = %email.message_id,
                    decision = ?decision.decision,
                    "github triage returned non-message decision kind; treating as skip"
                );
                self.store
                    .mark_email_processed(&email.message_id, TriageResult::Skip)?;
                outcome.priority_skipped += 1;
                Ok(())
            }
        }
    }

    /// Best-effort PATCH to `/notifications/threads/{id}`. We only log on
    /// failure — leaving the thread unread will cause re-triage on the next
    /// poll (caught by `is_message_processed`), so it's a soft cost.
    async fn maybe_mark_read(&self, thread_id: u64) {
        if !self.config.mark_read_on_dispatch || self.config.dry_run {
            return;
        }
        if let Err(e) = self.api.mark_thread_read(thread_id).await {
            debug!(thread_id, "mark_thread_read failed: {e:#}");
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

/// Build a `TriageCandidate` from the wire pieces. Body falls back to title
/// when the linked subject can't be hydrated (e.g. CI activity).
fn build_candidate(
    notif: Notification,
    kind: &'static str,
    detail: Option<SubjectDetail>,
) -> TriageCandidate {
    let thread_id_u64 = notif.thread_id_u64().unwrap_or(0);
    let locator = notif.thread_locator();
    let repo_full_name = notif.repository.full_name.clone();
    let title = notif.subject.title.clone();
    let updated_at = notif.updated_at.clone();
    let (body, author_login, html_url) = match detail {
        Some(d) => (
            d.body.unwrap_or_else(|| title.clone()),
            d.user.login,
            d.html_url,
        ),
        None => (title.clone(), String::new(), notif.repository.html_url),
    };
    TriageCandidate {
        thread_id_u64,
        kind,
        locator,
        repo_full_name,
        title,
        body,
        author_login,
        html_url,
        updated_at,
    }
}

/// Pick the subscription mode for a `<owner>/<repo>`. Comparison is
/// case-insensitive on `channel_id` to match how a user might enter
/// `Octocat/Hello-World` on the CLI.
fn resolve_mode(
    repo_full_name: &str,
    subs: &[ChannelSubscription],
    default_mode: SubscriptionMode,
) -> SubscriptionMode {
    let needle = repo_full_name.to_ascii_lowercase();
    for s in subs {
        if s.channel_id.to_ascii_lowercase() == needle {
            return s.mode;
        }
    }
    default_mode
}

/// Pull `(owner, repo, number)` back out of a github-channel `Email`. Used by
/// the approver in `augmentagent-cli` on Approve.
pub fn outbound_target(email: &Email) -> Option<ThreadLocator> {
    ThreadLocator::from_thread_id(email.thread_id.as_deref()?)
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
    use augmentagent_channel_core::{Reasoner, ReasonerOpts};
    use augmentagent_store::Email;

    use crate::types::{Notification, NotificationSubject, Repository};

    struct StubApi {
        notifs: Vec<Notification>,
        marked_read: std::sync::Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl GithubApi for StubApi {
        async fn list_notifications(
            &self,
            _since_iso: Option<&str>,
            _all: bool,
        ) -> Result<Vec<Notification>, GithubError> {
            Ok(self.notifs.clone())
        }
        async fn fetch_subject(
            &self,
            _subject_url: &str,
        ) -> Result<Option<SubjectDetail>, GithubError> {
            Ok(Some(SubjectDetail {
                title: "Title".into(),
                body: Some("Hi @nolanmak, can you take a look?".into()),
                html_url: "https://example".into(),
                user: crate::types::SubjectUser {
                    login: "octocat".into(),
                },
            }))
        }
        async fn mark_thread_read(&self, thread_id: u64) -> Result<(), GithubError> {
            self.marked_read.lock().unwrap().push(thread_id);
            Ok(())
        }
        async fn post_issue_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _number: u64,
            _body: &str,
        ) -> Result<u64, GithubError> {
            Ok(123456789)
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
    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"decision":"skip","reason":"stub"}"#.into()))
        }
    }

    #[derive(Default)]
    struct RecordingBroker {
        approvals: std::sync::Mutex<Vec<String>>,
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
            self.approvals.lock().unwrap().push(id.to_string());
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
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn sample_notif(id: &str, reason: &str, full_name: &str, num: u64) -> Notification {
        Notification {
            id: id.into(),
            reason: reason.into(),
            unread: true,
            updated_at: "2026-05-14T12:00:00Z".into(),
            repository: Repository {
                full_name: full_name.into(),
                html_url: format!("https://github.com/{full_name}"),
            },
            subject: NotificationSubject {
                title: "Need eyes".into(),
                subject_type: "Issue".into(),
                url: format!(
                    "https://api.github.com/repos/{full_name}/issues/{num}"
                ),
            },
        }
    }

    #[tokio::test]
    async fn unsupported_reasons_are_filtered() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            notifs: vec![
                sample_notif("100", "subscribed", "a/b", 1),
                sample_notif("101", "comment", "a/b", 2),
                sample_notif("102", "ci_activity", "a/b", 3),
            ],
            marked_read: std::sync::Mutex::new(vec![]),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store,
            api,
            reasoner,
            broker.clone(),
            "nolanmak".into(),
            GithubChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/none"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.notifications_seen, 3);
        assert_eq!(out.filtered_out, 3);
        assert_eq!(out.priority_awaiting_approval, 0);
        assert!(broker.approvals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allowed_reasons_pass_through() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            notifs: vec![
                sample_notif("200", "mention", "a/b", 1),
                sample_notif("201", "review_requested", "a/b", 2),
                sample_notif("202", "assign", "a/b", 3),
                sample_notif("203", "subscribed-when-mentioned", "a/b", 4),
            ],
            marked_read: std::sync::Mutex::new(vec![]),
        });
        // 4 notifications × 1 triage call each, all "skip" so no draft pass.
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"noise"}"#,
            r#"{"decision":"skip","reason":"noise"}"#,
            r#"{"decision":"skip","reason":"noise"}"#,
            r#"{"decision":"skip","reason":"noise"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store,
            Arc::clone(&api),
            reasoner,
            broker.clone(),
            "nolanmak".into(),
            GithubChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/none"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.notifications_seen, 4);
        assert_eq!(out.filtered_out, 0);
        assert_eq!(out.priority_skipped, 4);
        // mark_read called once per dispatched skip.
        assert_eq!(api.marked_read.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn priority_reply_posts_approval_card() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            notifs: vec![sample_notif("300", "review_requested", "a/b", 7)],
            marked_read: std::sync::Mutex::new(vec![]),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"PR review request"}"#,
            "LGTM, ship it.",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store.clone(),
            Arc::clone(&api),
            reasoner,
            broker.clone(),
            "nolanmak".into(),
            GithubChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/none"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.priority_awaiting_approval, 1);
        assert_eq!(broker.approvals.lock().unwrap().len(), 1);
        // Don't mark read while pending — only on dispatch terminal moments.
        // (Reply dispatch leaves it for the approver to mark on Approve/Skip.)
        assert!(api.marked_read.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn priority_flag_posts_flag_notice() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            notifs: vec![sample_notif("400", "mention", "a/b", 9)],
            marked_read: std::sync::Mutex::new(vec![]),
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"flag","reason":"unclear request"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store,
            Arc::clone(&api),
            reasoner,
            broker.clone(),
            "nolanmak".into(),
            GithubChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/none"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.priority_flagged, 1);
        let flags = broker.flags.lock().unwrap();
        assert_eq!(flags.len(), 1);
        assert!(flags[0].1.contains("unclear request"));
    }

    #[tokio::test]
    async fn store_only_subscription_skips_triage() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            notifs: vec![sample_notif("500", "mention", "noisy/repo", 11)],
            marked_read: std::sync::Mutex::new(vec![]),
        });
        store
            .upsert_subscription(
                PLATFORM,
                "noisy/repo",
                "noisy/repo",
                SubscriptionMode::StoreOnly,
                None,
            )
            .unwrap();

        // Empty reasoner queue — if handle_priority is incorrectly invoked
        // it'll fall through to the default skip response and the test still
        // catches it via the outcome counts.
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store.clone(),
            Arc::clone(&api),
            reasoner.clone(),
            broker.clone(),
            "nolanmak".into(),
            GithubChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/none"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.store_only_stored, 1);
        assert_eq!(out.priority_awaiting_approval, 0);
        assert_eq!(out.priority_skipped, 0);
        assert!(broker.approvals.lock().unwrap().is_empty());
        // Reasoner should NOT have been called.
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn already_processed_notification_is_skipped() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            notifs: vec![sample_notif("600", "mention", "a/b", 13)],
            marked_read: std::sync::Mutex::new(vec![]),
        });
        // Pre-stamp the email + a logged action so the gate trips.
        let prior_email = Email {
            message_id: "gh:600".into(),
            thread_id: Some("a/b#13".into()),
            from: "octocat <github:octocat>".into(),
            subject: "[mention] a/b #13 — Need eyes".into(),
            body: "x".into(),
            date: "2026-05-14T12:00:00Z".into(),
            account_entity_id: Some("github:nolanmak".into()),
            platform: PLATFORM.into(),
            kind: "mention".into(),
        };
        store.upsert_email(&prior_email).unwrap();
        store
            .log_action(
                "gh:600",
                Some("a/b#13"),
                "octocat <github:octocat>",
                "[mention] a/b #13 — Need eyes",
                Some("x"),
                Some("prev-draft"),
                ActionStatus::Pending,
            )
            .unwrap();

        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store.clone(),
            Arc::clone(&api),
            reasoner.clone(),
            broker.clone(),
            "nolanmak".into(),
            GithubChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/none"),
                ..Default::default()
            },
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.already_processed, 1);
        assert_eq!(out.priority_awaiting_approval, 0);
        assert!(reasoner.responses.lock().unwrap().is_empty());
    }

    #[test]
    fn resolve_mode_uses_default_when_no_sub() {
        let m = resolve_mode("a/b", &[], SubscriptionMode::StoreOnly);
        assert_eq!(m, SubscriptionMode::StoreOnly);
    }

    #[test]
    fn resolve_mode_matches_case_insensitive() {
        let sub = ChannelSubscription {
            id: "1".into(),
            platform: PLATFORM.into(),
            channel_id: "octocat/hello-world".into(),
            display_name: "x".into(),
            mode: SubscriptionMode::Digest,
            active: true,
            account_id: None,
            last_seen_message_id: None,
            last_digest_at_ms: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let m = resolve_mode(
            "Octocat/Hello-World",
            std::slice::from_ref(&sub),
            SubscriptionMode::Priority,
        );
        assert_eq!(m, SubscriptionMode::Digest);
    }

    #[test]
    fn outbound_target_round_trips() {
        let email = Email {
            message_id: "gh:1".into(),
            thread_id: Some("octocat/Hello-World#7".into()),
            from: "x".into(),
            subject: "y".into(),
            body: "b".into(),
            date: "".into(),
            account_entity_id: Some("github:nolanmak".into()),
            platform: PLATFORM.into(),
            kind: "mention".into(),
        };
        let loc = outbound_target(&email).unwrap();
        assert_eq!(loc.owner, "octocat");
        assert_eq!(loc.repo, "Hello-World");
        assert_eq!(loc.number, 7);
    }

    #[tokio::test]
    async fn auth_invalid_returns_clean() {
        struct ExpiredApi;
        #[async_trait]
        impl GithubApi for ExpiredApi {
            async fn list_notifications(
                &self,
                _: Option<&str>,
                _: bool,
            ) -> Result<Vec<Notification>, GithubError> {
                Err(GithubError::AuthInvalid)
            }
            async fn fetch_subject(
                &self,
                _: &str,
            ) -> Result<Option<SubjectDetail>, GithubError> {
                Err(GithubError::AuthInvalid)
            }
            async fn mark_thread_read(&self, _: u64) -> Result<(), GithubError> {
                Err(GithubError::AuthInvalid)
            }
            async fn post_issue_comment(
                &self,
                _: &str,
                _: &str,
                _: u64,
                _: &str,
            ) -> Result<u64, GithubError> {
                Err(GithubError::AuthInvalid)
            }
        }
        let (store, _f) = tmp_store();
        let api = Arc::new(ExpiredApi);
        let reasoner = Arc::new(ScriptedReasoner::new([]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = GithubChannel::new(
            store,
            api,
            reasoner,
            broker,
            "nolanmak".into(),
            GithubChannelConfig::default(),
        );
        let out = ch.poll_once().await.unwrap();
        assert_eq!(out.errors, 1);
        assert_eq!(out.notifications_seen, 0);
    }
}
