//! Own-post comment-reply poller (#58.2).
//!
//! [`OwnPostsCommentTrigger`] is a [`Trigger`] that, on each tick, walks the
//! durable `own_posts` table (rows registered via
//! `augmentagent linkedin watch-post …` / the dashboard), fetches recent
//! comments on each post still inside its poll window, diffs them against the
//! store's `seen_comments` table, and yields one
//! `WorkItem { platform:"linkedin", kind:"own_post_comment" }` per genuinely
//! new comment.
//!
//! It produces *work items only* — the triage → draft → approval-card path is
//! [`OwnPostCommentEngagement`]'s job (mirrors how `LinkedInFeedEngagement`
//! handles friend posts). Every reply still requires Discord approval and is
//! RateGovernor-`Comment`-gated; nothing here auto-posts.
//!
//! Durability: `seen_comments` is the dedup ledger (a comment becomes a
//! WorkItem exactly once, ever, even across daemon restarts);
//! `own_posts.last_polled_ms` records cadence so the tick spreads load
//! (`own_posts_due_for_poll` orders least-recently-polled first).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::governor::{
    ActionKind, ActionRequest, Denial, Platform, RateGovernor, Risk,
};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{Trigger, WorkItem};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Store, TriageResult};

use crate::api::LinkedInApi;
use crate::channel::LinkedInChannelConfig;
use crate::types::PostComment;

/// Default own-post comment poll cadence. The #58 spec asks for tight cadence
/// early in a post's life; we keep a single conservative 30-min cadence (the
/// per-post 7-day `poll_until_ms` horizon handles "stop eventually") rather
/// than the spec's tiered 15-min/1-h schedule — simpler, still well within
/// LinkedIn's anti-scrape envelope, and the cap below bounds outbound anyway.
pub const DEFAULT_OWN_POST_POLL_SECS: u64 = 30 * 60;

/// Default per-day reply cap. Comment-replies are governed by the merged
/// RateGovernor too, but this is a cheap pre-filter so a viral post can't
/// flood the triage pipeline with hundreds of LLM calls in one tick.
pub const DEFAULT_MAX_REPLIES_PER_DAY: u32 = 10;

/// Serialized payload carried in `WorkItem.payload`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct OwnPostCommentPayload {
    pub post_urn: String,
    pub comment_urn: String,
    pub author_name: String,
    pub author_urn: String,
    pub text: String,
    pub created_at_ms: i64,
}

/// Polls own posts for new comments and yields `own_post_comment` work items.
pub struct OwnPostsCommentTrigger<L: LinkedInApi> {
    api: Arc<L>,
    store: Arc<Store>,
    max_per_day: u32,
}

impl<L: LinkedInApi> OwnPostsCommentTrigger<L> {
    pub fn new(api: Arc<L>, store: Arc<Store>, max_per_day: u32) -> Self {
        Self {
            api,
            store,
            max_per_day: max_per_day.max(1),
        }
    }
}

#[async_trait]
impl<L: LinkedInApi + 'static> Trigger for OwnPostsCommentTrigger<L> {
    async fn next_work_items(&self, cancel: &CancellationToken) -> anyhow::Result<Vec<WorkItem>> {
        let now_ms = now_millis();
        let posts = self.store.own_posts_due_for_poll("linkedin", now_ms)?;
        if posts.is_empty() {
            debug!("own-post comment poller: no posts in poll window");
            return Ok(Vec::new());
        }
        let mut budget = self.max_per_day;
        let mut out = Vec::new();
        for post in posts {
            if cancel.is_cancelled() || budget == 0 {
                break;
            }
            let comments = match self.api.fetch_post_comments(&post.external_id).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(post = %post.external_id, error = %e, "comment fetch failed; skipping post");
                    continue;
                }
            };
            for c in comments {
                if budget == 0 {
                    break;
                }
                // Durable one-shot dedup: record_seen_comment returns false
                // if this (post, comment) was already seen.
                let is_new = self.store.record_seen_comment(
                    &post.id,
                    &c.comment_urn,
                    Some(c.author_name.as_str()),
                    Some(c.text.as_str()),
                )?;
                if !is_new {
                    continue;
                }
                out.push(to_work_item(&c));
                budget -= 1;
            }
            if let Err(e) = self.store.mark_own_post_polled(&post.id) {
                warn!(post = %post.id, "mark_own_post_polled failed: {e}");
            }
        }
        Ok(out)
    }
}

fn to_work_item(c: &PostComment) -> WorkItem {
    let payload = OwnPostCommentPayload {
        post_urn: c.post_urn.clone(),
        comment_urn: c.comment_urn.clone(),
        author_name: c.author_name.clone(),
        author_urn: c.author_urn.0.clone(),
        text: c.text.clone(),
        created_at_ms: c.created_at_ms,
    };
    WorkItem {
        platform: "linkedin".into(),
        kind: augmentagent_channel_core::work_item_kind::OWN_POST_COMMENT.into(),
        external_id: c.comment_urn.clone(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Drives [`OwnPostsCommentTrigger`] on a cadence and runs each surfaced
/// comment through triage → draft → approval-card. Approve → the approver
/// posts the reply (no auto-posting). Every dispatch is wrapped in the merged
/// RateGovernor `Comment` permit/record envelope; a soft denial defers to the
/// next tick (the comment is already recorded in `seen_comments`, so it is
/// not lost — it just won't re-surface, which is correct: a deferred reply
/// stays the user's call via the dashboard).
pub struct OwnPostCommentEngagement<L: LinkedInApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub governor: Arc<dyn RateGovernor>,
    pub trigger: Arc<OwnPostsCommentTrigger<L>>,
    pub member_urn: String,
    pub config: LinkedInChannelConfig,
    pub poll_interval: Duration,
}

impl<L: LinkedInApi + 'static, R: Reasoner + 'static> OwnPostCommentEngagement<L, R> {
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            interval_secs = self.poll_interval.as_secs(),
            "own-post comment engagement started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("own-post comment engagement: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once(&shutdown).await {
                        Ok(n) => info!(replied = n, "own-post comment poll complete"),
                        Err(e) => error!("own-post comment poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One poll: ask the trigger for fresh comments, triage + draft a reply
    /// for each, post an approval card. Returns the count of approval cards
    /// posted.
    pub async fn poll_once(&self, cancel: &CancellationToken) -> anyhow::Result<usize> {
        let items = self.trigger.next_work_items(cancel).await?;
        let mut posted = 0usize;
        for item in items {
            let payload: OwnPostCommentPayload = match serde_json::from_value(item.payload.clone())
            {
                Ok(p) => p,
                Err(e) => {
                    warn!("own-post comment payload decode failed: {e}");
                    continue;
                }
            };
            match self.handle_comment(payload).await {
                Ok(true) => posted += 1,
                Ok(false) => {}
                Err(e) => error!("handle_comment failed: {e:#}"),
            }
        }
        Ok(posted)
    }

    async fn handle_comment(&self, payload: OwnPostCommentPayload) -> anyhow::Result<bool> {
        let comment = PostComment {
            post_urn: payload.post_urn,
            comment_urn: payload.comment_urn,
            author_name: payload.author_name,
            author_urn: crate::types::MemberUrn(payload.author_urn),
            text: payload.text,
            created_at_ms: payload.created_at_ms,
        };
        let email = comment.into_email(&self.member_urn);
        self.store.upsert_email(&email)?;
        if self.store.is_message_processed(&email.message_id)? {
            return Ok(false);
        }

        let triage_opts = triage_opts(self.config.wiki_root.clone());
        let triage_prompt = triage_user_message(&email, "", "");
        let raw = self.reasoner.call(&triage_opts, &triage_prompt).await?;
        let decision = match parse_decision(&raw) {
            Ok(d) => d,
            Err(e) => {
                error!(message_id = %email.message_id, "comment triage parse failed: {e}; raw={raw}");
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

        if !matches!(decision.decision, DecisionKind::Reply) {
            // Spam / emoji-only / not-worth-a-reply → record + skip (the #58
            // cheap-pass: triage is the filter; we never reply to noise).
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
            return Ok(false);
        }

        // Governor preflight — a Comment is always approval-gated by the
        // matrix; the #58 approval card *is* that approval, so we treat
        // ApprovalRequired as "the card covers it" and proceed. Hard/soft
        // caps defer to the next tick (the comment is already in
        // seen_comments; not lost — the user can act from the dashboard).
        let permit = if let Some(plat) = Platform::parse("linkedin") {
            let req = ActionRequest {
                platform: plat,
                action: ActionKind::Comment,
                account_id: format!("linkedin:{}", self.member_urn),
                risk: Risk::Low,
                cause: format!("own_post_comment:{}", email.message_id),
                target_id: Some(email.message_id.clone()),
                target_attrs: None,
            };
            match self.governor.permit(req).await {
                Ok(p) => Some(p),
                Err(Denial::ApprovalRequired { .. }) => None,
                Err(d) => {
                    info!(
                        comment = %email.message_id,
                        "own-post reply deferred by governor: {d}"
                    );
                    return Ok(false);
                }
            }
        } else {
            None
        };

        let skill_system =
            std::fs::read_to_string(self.config.skill_dir.join("SKILL.md")).unwrap_or_default();
        let draft_opts = draft_opts(skill_system, self.config.wiki_root.clone());
        let draft_prompt = draft_user_message(&email, "", "", "", "", "");
        let draft = self
            .reasoner
            .call(&draft_opts, &draft_prompt)
            .await?
            .trim()
            .to_string();

        if self.config.dry_run {
            if let Some(p) = permit {
                let _ = self
                    .governor
                    .record(p, augmentagent_channel_core::governor::Outcome::RolledBack)
                    .await;
            }
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
                "[linkedin own-post reply dry-run] {}\n--- reply ---\n{}\n--- /reply ---",
                email.subject, draft
            );
            return Ok(false);
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
            if let Some(p) = permit {
                let _ = self
                    .governor
                    .record(p, augmentagent_channel_core::governor::Outcome::RolledBack)
                    .await;
            }
            self.store.update_action_status(
                &action_id,
                ActionStatus::Error,
                None,
                Some(&format!("post_approval: {e}")),
            )?;
            return Err(anyhow::anyhow!("post_approval: {e}"));
        }
        // Card surfaced — record the permit as Ok (quota is consumed at the
        // point the user is asked to approve; the actual send is the
        // approver's job and re-permits there if it ever needs to).
        if let Some(p) = permit {
            let _ = self
                .governor
                .record(p, augmentagent_channel_core::governor::Outcome::Ok)
                .await;
        }
        info!(action_id, comment = %email.message_id, "own-post reply card posted");
        Ok(true)
    }
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
    use crate::api::LinkedInError;
    use crate::posting::{PostDraft, ShareUrn};
    use crate::types::{Dm, FeedPost, Invitation, MemberUrn};
    use std::path::PathBuf;

    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::governor::{Outcome, Permit};
    use augmentagent_channel_core::{Reasoner, ReasonerOpts};
    use augmentagent_store::Email;

    struct StubApi {
        comments: Vec<PostComment>,
    }
    #[async_trait]
    impl LinkedInApi for StubApi {
        async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
            Ok(vec![])
        }
        async fn send_message(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
            Ok("urn:li:messagingMessage:STUB".into())
        }
        async fn fetch_feed_posts_by_author(
            &self,
            _: &str,
        ) -> Result<Vec<FeedPost>, LinkedInError> {
            Ok(vec![])
        }
        async fn post_comment(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
            Ok("urn:li:comment:STUB".into())
        }
        async fn react(&self, _: &str, _: &str) -> Result<(), LinkedInError> {
            Ok(())
        }
        async fn create_share(&self, _: PostDraft<'_>) -> Result<ShareUrn, LinkedInError> {
            Ok(ShareUrn("urn:li:share:STUB".into()))
        }
        async fn fetch_post_comments(
            &self,
            _post_urn: &str,
        ) -> Result<Vec<PostComment>, LinkedInError> {
            Ok(self.comments.clone())
        }
        async fn fetch_pending_invitations(&self) -> Result<Vec<Invitation>, LinkedInError> {
            Ok(vec![])
        }
        async fn act_on_invitation(&self, _: &str, _: bool) -> Result<(), LinkedInError> {
            Ok(())
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
        async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
            let mut q = self.responses.lock().unwrap();
            Ok(q.pop_front()
                .unwrap_or_else(|| "{\"decision\":\"skip\",\"reason\":\"stub\"}".into()))
        }
    }

    #[derive(Default)]
    struct RecordingBroker {
        posts: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(
            &self,
            action_id: &str,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
            self.posts.lock().unwrap().push(action_id.to_string());
            Ok(())
        }
        async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
            Ok(())
        }
    }

    struct AlwaysPermit;
    #[async_trait]
    impl RateGovernor for AlwaysPermit {
        async fn permit(&self, req: ActionRequest) -> Result<Permit, Denial> {
            Ok(Permit {
                id: uuid::Uuid::new_v4(),
                req,
                reserved_at_ms: 0,
            })
        }
        async fn record(&self, _: Permit, _: Outcome) -> anyhow::Result<()> {
            Ok(())
        }
        async fn record_halt(
            &self,
            _: Platform,
            _: augmentagent_channel_core::governor::HaltReason,
            _: i64,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn halt_status(
            &self,
            _: Platform,
        ) -> Option<augmentagent_channel_core::governor::HaltState> {
            None
        }
        async fn is_halted(&self, _: Platform) -> Option<i64> {
            None
        }
    }

    /// Governor that defers everything with a hard daily cap denial.
    struct AlwaysDeny;
    #[async_trait]
    impl RateGovernor for AlwaysDeny {
        async fn permit(&self, _: ActionRequest) -> Result<Permit, Denial> {
            Err(Denial::DailyCap {
                platform: Platform::LinkedIn,
                action: ActionKind::Comment,
                used: 99,
                cap: 99,
            })
        }
        async fn record(&self, _: Permit, _: Outcome) -> anyhow::Result<()> {
            Ok(())
        }
        async fn record_halt(
            &self,
            _: Platform,
            _: augmentagent_channel_core::governor::HaltReason,
            _: i64,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn halt_status(
            &self,
            _: Platform,
        ) -> Option<augmentagent_channel_core::governor::HaltState> {
            None
        }
        async fn is_halted(&self, _: Platform) -> Option<i64> {
            None
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
                    agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn comment(urn: &str) -> PostComment {
        PostComment {
            post_urn: "urn:li:activity:1".into(),
            comment_urn: urn.into(),
            author_name: "Jane Doe".into(),
            author_urn: MemberUrn("urn:li:fsd_profile:JANE".into()),
            text: "Congrats on shipping!".into(),
            created_at_ms: 1_776_630_000_000,
        }
    }

    fn engagement(
        store: Arc<Store>,
        api: Arc<StubApi>,
        reasoner: Arc<ScriptedReasoner>,
        broker: Arc<RecordingBroker>,
        governor: Arc<dyn RateGovernor>,
        dry_run: bool,
    ) -> OwnPostCommentEngagement<StubApi, ScriptedReasoner> {
        let trigger = Arc::new(OwnPostsCommentTrigger::new(
            api,
            Arc::clone(&store),
            DEFAULT_MAX_REPLIES_PER_DAY,
        ));
        OwnPostCommentEngagement {
            store,
            reasoner,
            approvals: broker,
            governor,
            trigger,
            member_urn: "urn:li:fsd_profile:ME".into(),
            config: LinkedInChannelConfig {
                dry_run,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
            poll_interval: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn trigger_yields_new_comments_once_then_dedups() {
        let (store, _f) = tmp_store();
        store
            .upsert_own_post(
                "linkedin",
                "urn:li:activity:1",
                1_776_000_000_000,
                now_millis() + 86_400_000,
            )
            .unwrap();
        let api = Arc::new(StubApi {
            comments: vec![comment("urn:li:comment:1"), comment("urn:li:comment:2")],
        });
        let trig = OwnPostsCommentTrigger::new(api, Arc::clone(&store), 10);
        let cancel = CancellationToken::new();
        let first = trig.next_work_items(&cancel).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            first[0].kind,
            augmentagent_channel_core::work_item_kind::OWN_POST_COMMENT
        );
        // Second poll → all already in seen_comments → empty.
        let second = trig.next_work_items(&cancel).await.unwrap();
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn reply_decision_posts_approval_card() {
        let (store, _f) = tmp_store();
        store
            .upsert_own_post(
                "linkedin",
                "urn:li:activity:1",
                1_776_000_000_000,
                now_millis() + 86_400_000,
            )
            .unwrap();
        let api = Arc::new(StubApi {
            comments: vec![comment("urn:li:comment:1")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"thoughtful comment"}"#,
            "Thanks so much, Jane!",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let eng = engagement(
            Arc::clone(&store),
            api,
            reasoner,
            Arc::clone(&broker),
            Arc::new(AlwaysPermit),
            false,
        );
        let n = eng.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn skip_decision_posts_no_card() {
        let (store, _f) = tmp_store();
        store
            .upsert_own_post(
                "linkedin",
                "urn:li:activity:1",
                1_776_000_000_000,
                now_millis() + 86_400_000,
            )
            .unwrap();
        let api = Arc::new(StubApi {
            comments: vec![comment("urn:li:comment:spam")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"emoji-only spam"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let eng = engagement(
            Arc::clone(&store),
            api,
            reasoner,
            Arc::clone(&broker),
            Arc::new(AlwaysPermit),
            false,
        );
        let n = eng.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 0);
        assert!(broker.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn governor_hard_denial_defers_no_card() {
        let (store, _f) = tmp_store();
        store
            .upsert_own_post(
                "linkedin",
                "urn:li:activity:1",
                1_776_000_000_000,
                now_millis() + 86_400_000,
            )
            .unwrap();
        let api = Arc::new(StubApi {
            comments: vec![comment("urn:li:comment:1")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"worth replying"}"#,
            "Thanks!",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let eng = engagement(
            Arc::clone(&store),
            api,
            reasoner,
            Arc::clone(&broker),
            Arc::new(AlwaysDeny),
            false,
        );
        let n = eng.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 0);
        assert!(broker.posts.lock().unwrap().is_empty());
    }
}
