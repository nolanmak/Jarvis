//! Watchlist-driven friend-post engagement (#58.3).
//!
//! Distinct from the pre-existing wiki-`close:`-driven [`LinkedInFeedTrigger`]
//! (#13): this is the #58 spine's `FriendFeedSource` implementation — it
//! iterates the durable `friend_watchlist` table (managed via
//! `augmentagent linkedin friend-watch …` / the dashboard), fetches each
//! watched friend's recent posts via Voyager, dedups against
//! `friend_posts_seen`, and yields one
//! `WorkItem { kind:"friend_post" }` per genuinely new post.
//!
//! The two coexist deliberately: #13's trigger keys off wiki front-matter
//! (zero-config for anyone who already maintains `wiki/people/*.md`); #58.3's
//! source keys off an explicit table so a friend can be watched without a
//! wiki page and with a per-friend `engagement` tier. Both feed the same
//! triage → wiki-grounded-draft → approval-card path and the same
//! RateGovernor `Comment` envelope; nothing here auto-posts.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::governor::{
    ActionKind, ActionRequest, Denial, Outcome, Platform, RateGovernor, Risk,
};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{FriendFeedSource, WorkItem};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Store, TriageResult};

use crate::api::LinkedInApi;
use crate::channel::LinkedInChannelConfig;
use crate::types::FeedPost;

/// Default watchlist-feed poll cadence: 6h (same anti-fingerprint posture as
/// the #13 feed trigger — LinkedIn's heuristics care about request
/// regularity).
pub const DEFAULT_FRIEND_FEED_POLL_SECS: u64 = 6 * 60 * 60;

/// Default per-tick post budget. The RateGovernor `Comment` envelope is the
/// real cap; this just bounds how many LLM triage calls one tick can spawn.
pub const DEFAULT_MAX_FRIEND_POSTS_PER_TICK: u32 = 8;

/// Milestone keywords used to gate `engagement = 'low'` watches — only
/// surface a low-tier friend's post if it reads like a real life/work event.
const MILESTONE_KEYWORDS: &[&str] = &[
    "raising", "raised", "launching", "launched", "joined", "hiring",
    "excited to announce", "new role", "new job", "acquired", "shipped",
];

/// Serialized payload carried in `WorkItem.payload`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FriendPostPayload {
    pub post_urn: String,
    pub author_name: String,
    pub author_urn: String,
    pub text: String,
    pub created_at_ms: i64,
    /// Wiki slug of the watched friend (if any) — grounds the draft prompt.
    pub wiki_slug: Option<String>,
    /// `high` | `medium` | `low` — the per-friend engagement tier.
    pub engagement: String,
}

/// `friend_watchlist`-driven [`FriendFeedSource`] for LinkedIn.
pub struct LinkedInFriendFeedSource<L: LinkedInApi> {
    api: Arc<L>,
    store: Arc<Store>,
    max_per_tick: u32,
}

impl<L: LinkedInApi> LinkedInFriendFeedSource<L> {
    pub fn new(api: Arc<L>, store: Arc<Store>, max_per_tick: u32) -> Self {
        Self {
            api,
            store,
            max_per_tick: max_per_tick.max(1),
        }
    }
}

/// `true` if `text` reads like a milestone (gates `engagement = 'low'`).
fn looks_like_milestone(text: &str) -> bool {
    let lc = text.to_lowercase();
    MILESTONE_KEYWORDS.iter().any(|k| lc.contains(k))
}

#[async_trait]
impl<L: LinkedInApi + 'static> FriendFeedSource for LinkedInFriendFeedSource<L> {
    async fn fetch_new_friend_posts(&self) -> anyhow::Result<Vec<WorkItem>> {
        let now_ms = now_millis();
        let watch = self.store.active_friend_watch("linkedin", now_ms)?;
        if watch.is_empty() {
            debug!("friend-feed source: empty linkedin watchlist");
            return Ok(Vec::new());
        }
        let mut budget = self.max_per_tick;
        let mut out = Vec::new();
        for w in watch {
            if budget == 0 {
                break;
            }
            let posts = match self.api.fetch_feed_posts_by_author(&w.handle).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(handle = %w.handle, error = %e, "friend feed fetch failed; skipping");
                    continue;
                }
            };
            for post in posts {
                if budget == 0 {
                    break;
                }
                // 'low' tier: only surface milestone-ish posts.
                if w.engagement == "low" && !looks_like_milestone(&post.text) {
                    continue;
                }
                // Durable one-shot dedup.
                let is_new = self.store.record_friend_post_seen(
                    &w.id,
                    &post.post_urn,
                    post.created_at_ms,
                )?;
                if !is_new {
                    continue;
                }
                out.push(to_work_item(&post, &w.wiki_slug, &w.engagement));
                budget -= 1;
            }
        }
        Ok(out)
    }
}

fn to_work_item(
    post: &FeedPost,
    wiki_slug: &Option<String>,
    engagement: &str,
) -> WorkItem {
    let payload = FriendPostPayload {
        post_urn: post.post_urn.clone(),
        author_name: post.author_name.clone(),
        author_urn: post.author_urn.0.clone(),
        text: post.text.clone(),
        created_at_ms: post.created_at_ms,
        wiki_slug: wiki_slug.clone(),
        engagement: engagement.to_string(),
    };
    WorkItem {
        platform: "linkedin".into(),
        kind: augmentagent_channel_core::work_item_kind::FRIEND_POST.into(),
        external_id: post.post_urn.clone(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Drives [`LinkedInFriendFeedSource`] on a 6h cadence and runs each surfaced
/// friend post through triage → wiki-grounded-draft → approval-card. Approve
/// → the approver posts the comment (no auto-posting). Every dispatch is
/// wrapped in the merged RateGovernor `Comment` permit/record envelope; a
/// denial defers (the post is already recorded in `friend_posts_seen`).
pub struct FriendFeedEngagement<L: LinkedInApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub governor: Arc<dyn RateGovernor>,
    pub source: Arc<LinkedInFriendFeedSource<L>>,
    pub member_urn: String,
    pub config: LinkedInChannelConfig,
    pub poll_interval: Duration,
}

impl<L: LinkedInApi + 'static, R: Reasoner + 'static> FriendFeedEngagement<L, R> {
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            interval_secs = self.poll_interval.as_secs(),
            "friend-feed engagement started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("friend-feed engagement: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(n) => info!(engaged = n, "friend-feed poll complete"),
                        Err(e) => error!("friend-feed poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One poll: ask the source for fresh posts, triage + draft a supportive
    /// comment for each, post an approval card. Returns cards posted.
    pub async fn poll_once(&self) -> anyhow::Result<usize> {
        let items = self.source.fetch_new_friend_posts().await?;
        let mut posted = 0usize;
        for item in items {
            let payload: FriendPostPayload =
                match serde_json::from_value(item.payload.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("friend-post payload decode failed: {e}");
                        continue;
                    }
                };
            match self.handle_post(payload).await {
                Ok(true) => posted += 1,
                Ok(false) => {}
                Err(e) => error!("handle_post failed: {e:#}"),
            }
        }
        Ok(posted)
    }

    async fn handle_post(&self, payload: FriendPostPayload) -> anyhow::Result<bool> {
        let post = FeedPost {
            post_urn: payload.post_urn,
            author_name: payload.author_name,
            author_urn: crate::types::MemberUrn(payload.author_urn),
            text: payload.text,
            created_at_ms: payload.created_at_ms,
        };
        let mut email = post.into_email(&self.member_urn);
        // #58.3 taxonomy: stamp the friend_post kind (FeedPost::into_email
        // defaults to the #13 `post_engagement` kind).
        email.kind =
            augmentagent_channel_core::work_item_kind::FRIEND_POST.to_string();
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
                error!(message_id = %email.message_id, "friend-post triage parse failed: {e}; raw={raw}");
                self.store.log_action(
                    &email.message_id,
                    None,
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
            self.store.log_action(
                &email.message_id,
                None,
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

        let permit = if let Some(plat) = Platform::parse("linkedin") {
            let req = ActionRequest {
                platform: plat,
                action: ActionKind::Comment,
                account_id: format!("linkedin:{}", self.member_urn),
                risk: Risk::Low,
                cause: format!("friend_post:{}", email.message_id),
                target_id: Some(email.message_id.clone()),
                target_attrs: None,
            };
            match self.governor.permit(req).await {
                Ok(p) => Some(p),
                Err(Denial::ApprovalRequired { .. }) => None,
                Err(d) => {
                    info!(post = %email.message_id, "friend-post engagement deferred by governor: {d}");
                    return Ok(false);
                }
            }
        } else {
            None
        };

        // Wiki-grounded draft: triage_opts/draft_opts already pass
        // wiki_root, so the reasoner can pull wiki/people/<slug>.md into
        // context — the #58 "killer feature" (generic engagement reads as
        // spam; wiki-grounded reads as personal).
        let skill_system =
            std::fs::read_to_string(self.config.skill_dir.join("SKILL.md"))
                .unwrap_or_default();
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
                let _ = self.governor.record(p, Outcome::RolledBack).await;
            }
            self.store.log_action(
                &email.message_id,
                None,
                &email.from,
                &email.subject,
                Some(&email.body),
                Some(&draft),
                ActionStatus::DryRun,
            )?;
            self.store
                .mark_email_processed(&email.message_id, TriageResult::Reply)?;
            println!(
                "[linkedin friend-post dry-run] {}\n--- comment ---\n{}\n--- /comment ---",
                email.subject, draft
            );
            return Ok(false);
        }

        let action_id = self.store.log_action(
            &email.message_id,
            None,
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
                let _ = self.governor.record(p, Outcome::RolledBack).await;
            }
            self.store.update_action_status(
                &action_id,
                ActionStatus::Error,
                None,
                Some(&format!("post_approval: {e}")),
            )?;
            return Err(anyhow::anyhow!("post_approval: {e}"));
        }
        if let Some(p) = permit {
            let _ = self.governor.record(p, Outcome::Ok).await;
        }
        info!(action_id, post = %email.message_id, "friend-post engagement card posted");
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
    use crate::types::{Dm, Invitation, MemberUrn, PostComment};
    use std::path::PathBuf;

    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::governor::{Outcome, Permit};
    use augmentagent_channel_core::{Reasoner, ReasonerOpts};
    use augmentagent_store::Email;

    struct StubApi {
        posts: Vec<FeedPost>,
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
            Ok(self.posts.clone())
        }
        async fn post_comment(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
            Ok("urn:li:comment:STUB".into())
        }
        async fn react(&self, _: &str, _: &str) -> Result<(), LinkedInError> {
            Ok(())
        }
        async fn create_share(
            &self,
            _: PostDraft<'_>,
        ) -> Result<ShareUrn, LinkedInError> {
            Ok(ShareUrn("urn:li:share:STUB".into()))
        }
        async fn fetch_post_comments(
            &self,
            _: &str,
        ) -> Result<Vec<PostComment>, LinkedInError> {
            Ok(vec![])
        }
        async fn fetch_pending_invitations(
            &self,
        ) -> Result<Vec<Invitation>, LinkedInError> {
            Ok(vec![])
        }
        async fn act_on_invitation(
            &self,
            _: &str,
            _: bool,
        ) -> Result<(), LinkedInError> {
            Ok(())
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
        async fn post_flag_notice(
            &self,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
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

    fn post(urn: &str, text: &str) -> FeedPost {
        FeedPost {
            post_urn: urn.into(),
            author_name: "Alex Roe".into(),
            author_urn: MemberUrn("urn:li:fsd_profile:ALEX".into()),
            text: text.into(),
            created_at_ms: 1_776_630_000_000,
        }
    }

    #[tokio::test]
    async fn source_yields_new_posts_and_dedups() {
        let (store, _f) = tmp_store();
        store
            .upsert_friend_watch(
                "linkedin",
                "urn:li:fsd_profile:ALEX",
                Some("alex"),
                "high",
            )
            .unwrap();
        let api = Arc::new(StubApi {
            posts: vec![post("urn:li:activity:1", "hi"), post("urn:li:activity:2", "yo")],
        });
        let src =
            LinkedInFriendFeedSource::new(api, Arc::clone(&store), 10);
        let first = src.fetch_new_friend_posts().await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            first[0].kind,
            augmentagent_channel_core::work_item_kind::FRIEND_POST
        );
        let again = src.fetch_new_friend_posts().await.unwrap();
        assert!(again.is_empty(), "seen posts must not re-yield");
    }

    #[tokio::test]
    async fn low_tier_only_surfaces_milestone_posts() {
        let (store, _f) = tmp_store();
        store
            .upsert_friend_watch("linkedin", "urn:li:fsd_profile:ALEX", None, "low")
            .unwrap();
        let api = Arc::new(StubApi {
            posts: vec![
                post("urn:li:activity:1", "had a nice coffee today"),
                post("urn:li:activity:2", "Excited to announce we raised our seed!"),
            ],
        });
        let src = LinkedInFriendFeedSource::new(api, Arc::clone(&store), 10);
        let items = src.fetch_new_friend_posts().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "urn:li:activity:2");
    }

    #[tokio::test]
    async fn engagement_reply_posts_card() {
        let (store, _f) = tmp_store();
        store
            .upsert_friend_watch(
                "linkedin",
                "urn:li:fsd_profile:ALEX",
                Some("alex"),
                "high",
            )
            .unwrap();
        let api = Arc::new(StubApi {
            posts: vec![post("urn:li:activity:1", "We shipped v2!")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"genuine milestone"}"#,
            "Huge — congrats on v2, Alex!",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let src = Arc::new(LinkedInFriendFeedSource::new(
            api,
            Arc::clone(&store),
            10,
        ));
        let eng = FriendFeedEngagement {
            store: Arc::clone(&store),
            reasoner,
            approvals: Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            governor: Arc::new(AlwaysPermit),
            source: src,
            member_urn: "urn:li:fsd_profile:ME".into(),
            config: LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
            poll_interval: Duration::from_secs(1),
        };
        let n = eng.poll_once().await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }
}
