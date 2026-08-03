//! Own-post comment-reply poller for SocialAPI.ai (#243).
//!
//! [`SocialApiOwnPostCommentTrigger`] is a [`Trigger`] that, on each tick,
//! walks the durable `own_posts` table (rows with `platform = "socialapi"`),
//! lists inbox comments for each active SocialAPI.ai account via
//! [`SocialApiClient::list_comments`], keeps only the comments that land on a
//! watched own post, diffs them against the store's `socialapi_seen_comments`
//! ledger, and yields one
//! `WorkItem { platform:"socialapi", kind:"own_post_comment" }` per genuinely
//! new comment.
//!
//! It produces *work items only* — the triage → draft → approval-card path is
//! [`SocialApiOwnPostCommentEngagement`]'s job (mirrors the LinkedIn own-post
//! engagement). Every reply requires Discord approval; the send happens when
//! the operator approves, in the CLI's `approve_socialapi` (#244, merged).
//! Nothing here auto-posts.
//!
//! Durability: `socialapi_seen_comments` is the dedup ledger (a `(post,
//! comment)` pair becomes a WorkItem exactly once, ever, even across daemon
//! restarts). Note `own_posts.last_polled_ms` does NOT throttle anything —
//! `own_posts_due_for_poll` only uses it to ORDER BY least-recently-polled, so
//! every watched post is re-examined on every tick. `list_comments` likewise
//! returns the whole account inbox regardless of the watched set, so watching
//! more posts costs no extra API calls.

use std::collections::HashSet;
use std::path::PathBuf;
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
use augmentagent_channel_core::trigger::kind as work_item_kind;
use augmentagent_channel_core::trigger::{Trigger, WorkItem};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Email, Store, TriageResult};

use crate::client::SocialApiClient;
use crate::types::Comment;
use crate::PLATFORM;

/// Default own-post comment poll cadence (30 min) — matches the LinkedIn
/// engagement's conservative single cadence. The per-post `poll_until_ms`
/// horizon (set when the post is registered) handles "stop eventually".
pub const DEFAULT_OWN_POST_POLL_SECS: u64 = 30 * 60;

/// Default per-**tick** comment pre-cap. A cheap pre-filter so a viral post
/// can't flood the triage pipeline with hundreds of LLM calls in one tick.
///
/// #532: this was named `DEFAULT_MAX_REPLIES_PER_DAY` and documented as a
/// daily cap, but the budget is reinitialized on every `next_work_items` call
/// and nothing persists a daily counter — at the 30-min default cadence the
/// real ceiling was ~48x the documented one. Renamed rather than made
/// per-day: a durable daily ledger is a separate change, and the honest name
/// is what lets an operator size `AUGMENTAGENT_SOCIALAPI_MAX_OWNPOST_REPLIES`
/// correctly today.
///
/// Note the `RateGovernor` `Comment` envelope is NOT a backstop here:
/// `Platform::parse("socialapi")` returns `None`, so the governor path is
/// inert for this channel and this pre-cap is the only throttle.
pub const DEFAULT_MAX_COMMENTS_PER_TICK: u32 = 10;

/// Serialized payload carried in `WorkItem.payload`. The SocialAPI.ai
/// [`Comment`] plus the watched-post id it was matched against.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SocialApiOwnPostCommentPayload {
    pub post_id: String,
    pub comment_id: String,
    pub author: String,
    pub text: String,
    pub created_at: String,
}

/// Normalized comment webhook event body (#249) as persisted by the Express
/// receiver into `socialapi_webhook_events.payload_json`. Mirrors the
/// receiver's `normalizeSocialApiEvent` comment shape.
#[derive(Debug, Clone, serde::Deserialize)]
struct CommentWebhookPayload {
    /// Platform-native comment id (dedup key against `socialapi_seen_comments`).
    id: String,
    post_id: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    created_at: String,
}

impl From<CommentWebhookPayload> for SocialApiOwnPostCommentPayload {
    fn from(w: CommentWebhookPayload) -> Self {
        SocialApiOwnPostCommentPayload {
            post_id: w.post_id,
            comment_id: w.id,
            author: w.author,
            text: w.text,
            created_at: w.created_at,
        }
    }
}

impl SocialApiOwnPostCommentPayload {
    fn from_comment(c: &Comment) -> Self {
        Self {
            post_id: c.post_id.clone(),
            comment_id: c.id.clone(),
            author: c.author.clone(),
            text: c.text.clone(),
            created_at: c.created_at.clone(),
        }
    }

    /// Convert to the store's generic `Email` so the comment rides the same
    /// triage → draft → approval-card path as every other channel. `kind` is
    /// stamped `own_post_comment`; `thread_id` carries the parent post id so a
    /// later reply (issue #244) targets the right comment thread.
    fn into_email(self) -> Email {
        let from = format!("{} <socialapi:{}>", self.author, self.author);
        let subject = format!("[Comment on your post by {}]", self.author);
        Email {
            message_id: self.comment_id,
            thread_id: Some(self.post_id),
            from,
            subject,
            body: self.text,
            date: self.created_at,
            account_entity_id: Some(PLATFORM.to_string()),
            platform: PLATFORM.to_string(),
            kind: work_item_kind::OWN_POST_COMMENT.to_string(),
        }
    }
}

/// Polls watched SocialAPI.ai own posts for new comments and yields
/// `own_post_comment` work items.
pub struct SocialApiOwnPostCommentTrigger {
    client: Arc<SocialApiClient>,
    store: Arc<Store>,
    max_per_tick: u32,
}

impl SocialApiOwnPostCommentTrigger {
    pub fn new(client: Arc<SocialApiClient>, store: Arc<Store>, max_per_tick: u32) -> Self {
        Self {
            client,
            store,
            max_per_tick: max_per_tick.max(1),
        }
    }

    /// Fast-path drain of webhook-delivered comment events (#249). Reads up to
    /// `budget` unprocessed `socialapi_webhook_events` of kind `comment`, marks
    /// each processed, and — for comments landing on a `watched` own post and
    /// not already in `socialapi_seen_comments` — emits an `own_post_comment`
    /// WorkItem. Reusing the same dedup ledger as the poll path means a
    /// webhook-delivered comment and a later poll of the same comment collapse
    /// to a single draft. Comments on un-watched posts are still marked
    /// processed (so they don't wedge the queue) but produce no work — the poll
    /// path applies the identical watched-post filter. Best-effort: a malformed
    /// row is marked processed and skipped.
    fn drain_webhook_events(
        &self,
        watched: &HashSet<&str>,
        budget: u32,
    ) -> anyhow::Result<Vec<WorkItem>> {
        if budget == 0 {
            return Ok(Vec::new());
        }
        let events = self
            .store
            .take_unprocessed_socialapi_webhook_events("comment", budget)?;
        let mut out = Vec::new();
        for ev in events {
            if let Err(e) = self.store.mark_socialapi_webhook_event_processed(&ev.id) {
                warn!(event = %ev.id, "socialapi comment webhook: mark processed failed: {e}");
            }
            let wp: CommentWebhookPayload = match serde_json::from_str(&ev.payload_json) {
                Ok(p) => p,
                Err(e) => {
                    warn!(event = %ev.id, "socialapi comment webhook: payload decode failed: {e}");
                    continue;
                }
            };
            if !watched.contains(wp.post_id.as_str()) {
                continue;
            }
            let payload: SocialApiOwnPostCommentPayload = wp.into();
            // Durable one-shot dedup on (post_id, comment_id) — the SAME ledger
            // the poll path writes, so no double-draft.
            let is_new = self.store.record_seen_socialapi_comment(
                &payload.post_id,
                &payload.comment_id,
                Some(payload.author.as_str()),
                Some(payload.text.as_str()),
            )?;
            if !is_new {
                continue;
            }
            out.push(WorkItem {
                platform: PLATFORM.into(),
                kind: work_item_kind::OWN_POST_COMMENT.into(),
                external_id: payload.comment_id.clone(),
                payload: serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null),
            });
        }
        if !out.is_empty() {
            info!(n = out.len(), "socialapi own-post comment: drained webhook events (fast-path)");
        }
        Ok(out)
    }
}

#[async_trait]
impl Trigger for SocialApiOwnPostCommentTrigger {
    async fn next_work_items(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        let now_ms = now_millis();
        let posts = self.store.own_posts_due_for_poll(PLATFORM, now_ms)?;
        // The set of post ids we actually care about this tick; `list_comments`
        // returns the whole inbox per account, so we filter to watched posts.
        let watched: HashSet<&str> =
            posts.iter().map(|p| p.external_id.as_str()).collect();

        // #527: drain BEFORE the no-watched-posts early return. This used to
        // sit after it, so with nothing watched the pushed comment events that
        // this drain is the only thing that marks processed accumulated in
        // `socialapi_webhook_events` at processed=0 forever. Draining here
        // marks them regardless; dropping the un-watched ones matches the poll
        // path's filter and is safe because the poll re-lists the entire
        // account inbox every tick (no cursor), so a comment dropped now is
        // rediscovered once its post is registered.
        let mut budget = self.max_per_tick;
        let mut out = self.drain_webhook_events(&watched, budget)?;
        budget = budget.saturating_sub(out.len() as u32);

        if posts.is_empty() {
            debug!("socialapi own-post comment poller: no posts in poll window");
            return Ok(out);
        }

        let accounts = self.store.active_socialapi_account_ids()?;
        // No registered accounts → poll the whole inbox once (account_id=None).
        let scopes: Vec<Option<String>> = if accounts.is_empty() {
            vec![None]
        } else {
            accounts.into_iter().map(Some).collect()
        };

        for scope in scopes {
            if cancel.is_cancelled() || budget == 0 {
                break;
            }
            let comments = match self.client.list_comments(scope.as_deref()).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(account = ?scope, error = %e, "comment list failed; skipping account");
                    continue;
                }
            };
            for c in comments {
                if budget == 0 {
                    break;
                }
                if !watched.contains(c.post_id.as_str()) {
                    continue;
                }
                // Durable one-shot dedup keyed on (post_id, comment_id).
                let is_new = self.store.record_seen_socialapi_comment(
                    &c.post_id,
                    &c.id,
                    Some(c.author.as_str()),
                    Some(c.text.as_str()),
                )?;
                if !is_new {
                    continue;
                }
                out.push(to_work_item(&c));
                budget -= 1;
            }
        }

        // Stamp the cadence clock on every watched post we polled this tick.
        for post in &posts {
            if let Err(e) = self.store.mark_own_post_polled(&post.id) {
                warn!(post = %post.id, "mark_own_post_polled failed: {e}");
            }
        }
        Ok(out)
    }
}

fn to_work_item(c: &Comment) -> WorkItem {
    let payload = SocialApiOwnPostCommentPayload::from_comment(c);
    WorkItem {
        platform: PLATFORM.into(),
        kind: work_item_kind::OWN_POST_COMMENT.into(),
        external_id: c.id.clone(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Config for the SocialAPI.ai own-post comment engagement. Mirrors the subset
/// of LinkedIn's channel config the own-post handler actually reads.
#[derive(Debug, Clone)]
pub struct SocialApiOwnPostConfig {
    /// When true, drafts are logged (and the governor permit rolled back) but
    /// no approval card is posted.
    pub dry_run: bool,
    /// Wiki root passed into the triage/draft reasoner opts (grounding).
    pub wiki_root: Option<PathBuf>,
    /// Skill dir whose `SKILL.md` seeds the draft system prompt.
    pub skill_dir: PathBuf,
}

impl Default for SocialApiOwnPostConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            wiki_root: None,
            skill_dir: PathBuf::from("skills/email-triage"),
        }
    }
}

/// Drives [`SocialApiOwnPostCommentTrigger`] on a cadence and runs each
/// surfaced comment through triage → draft → approval-card. The actual reply
/// send happens on approve (#244); this stops at the approval card. Every dispatch is
/// wrapped in the merged RateGovernor `Comment` permit/record envelope.
pub struct SocialApiOwnPostCommentEngagement<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub governor: Arc<dyn RateGovernor>,
    pub trigger: Arc<SocialApiOwnPostCommentTrigger>,
    pub config: SocialApiOwnPostConfig,
    pub poll_interval: Duration,
}

impl<R: Reasoner + 'static> SocialApiOwnPostCommentEngagement<R> {
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            interval_secs = self.poll_interval.as_secs(),
            "socialapi own-post comment engagement started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("socialapi own-post comment engagement: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once(&shutdown).await {
                        Ok(n) => info!(carded = n, "socialapi own-post comment poll complete"),
                        Err(e) => error!("socialapi own-post comment poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One poll: ask the trigger for fresh comments, triage + draft a reply for
    /// each, post an approval card. Returns the count of approval cards posted.
    pub async fn poll_once(&self, cancel: &CancellationToken) -> anyhow::Result<usize> {
        let items = self.trigger.next_work_items(cancel).await?;
        let mut posted = 0usize;
        for item in items {
            let payload: SocialApiOwnPostCommentPayload =
                match serde_json::from_value(item.payload.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("socialapi own-post comment payload decode failed: {e}");
                        continue;
                    }
                };
            match self.handle_comment(payload).await {
                Ok(true) => posted += 1,
                Ok(false) => {}
                Err(e) => error!("socialapi handle_comment failed: {e:#}"),
            }
        }
        Ok(posted)
    }

    async fn handle_comment(
        &self,
        payload: SocialApiOwnPostCommentPayload,
    ) -> anyhow::Result<bool> {
        let email = payload.into_email();
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
            // Spam / emoji-only / not-worth-a-reply → record + skip. Triage is
            // the filter; we never reply to noise.
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

        // Governor preflight. SocialAPI.ai has no `Platform` rate-table rows
        // yet (`Platform::parse("socialapi")` is None), so this falls through
        // to "no permit, proceed" — the same fallback the LinkedIn handler
        // uses when a platform isn't matrixed. The approval card is the gate.
        let permit = if let Some(plat) = Platform::parse(PLATFORM) {
            let req = ActionRequest {
                platform: plat,
                action: ActionKind::Comment,
                account_id: format!("socialapi:{}", email.thread_id.clone().unwrap_or_default()),
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
                        "socialapi own-post reply deferred by governor: {d}"
                    );
                    return Ok(false);
                }
            }
        } else {
            None
        };

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
                let _ = self
                    .governor
                    .record(
                        p,
                        augmentagent_channel_core::governor::Outcome::RolledBack,
                    )
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
                "[socialapi own-post reply dry-run] {}\n--- reply ---\n{}\n--- /reply ---",
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
                    .record(
                        p,
                        augmentagent_channel_core::governor::Outcome::RolledBack,
                    )
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
        // Card surfaced — record the permit as Ok (quota consumed at the point
        // the user is asked to approve; the send happens on approve, #244).
        if let Some(p) = permit {
            let _ = self
                .governor
                .record(p, augmentagent_channel_core::governor::Outcome::Ok)
                .await;
        }
        info!(action_id, comment = %email.message_id, "socialapi own-post reply card posted");
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
    use crate::auth::SocialApiAuth;

    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::governor::{HaltReason, HaltState, Outcome, Permit};
    use augmentagent_channel_core::{Reasoner, ReasonerOpts};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> Arc<SocialApiClient> {
        Arc::new(SocialApiClient::with_base_url(
            SocialApiAuth::new("sk_test"),
            server.uri(),
        ))
    }

    /// Mounts `GET /inbox/comments` returning the supplied comment JSON array.
    async fn mount_comments(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/inbox/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
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
            _: HaltReason,
            _: i64,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn halt_status(&self, _: Platform) -> Option<HaltState> {
            None
        }
        async fn is_halted(&self, _: Platform) -> Option<i64> {
            None
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = Store::open(file.path()).unwrap();
        (Arc::new(store), file)
    }

    fn engagement(
        store: Arc<Store>,
        client: Arc<SocialApiClient>,
        reasoner: Arc<ScriptedReasoner>,
        broker: Arc<RecordingBroker>,
        dry_run: bool,
    ) -> SocialApiOwnPostCommentEngagement<ScriptedReasoner> {
        let trigger = Arc::new(SocialApiOwnPostCommentTrigger::new(
            client,
            Arc::clone(&store),
            DEFAULT_MAX_COMMENTS_PER_TICK,
        ));
        SocialApiOwnPostCommentEngagement {
            store,
            reasoner,
            approvals: broker,
            governor: Arc::new(AlwaysPermit),
            trigger,
            config: SocialApiOwnPostConfig {
                dry_run,
                wiki_root: None,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
            },
            poll_interval: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn trigger_yields_watched_new_comments_once_then_dedups() {
        let (store, _f) = tmp_store();
        let now = now_millis();
        store
            .upsert_own_post(PLATFORM, "post_1", now, now + 86_400_000)
            .unwrap();
        let server = MockServer::start().await;
        mount_comments(
            &server,
            serde_json::json!([
                {"id":"c1","post_id":"post_1","author":"jane","text":"nice!","created_at":"2026-05-28T00:00:00Z"},
                {"id":"c2","post_id":"post_1","author":"bob","text":"gg","created_at":"2026-05-28T00:01:00Z"},
                {"id":"c3","post_id":"other_post","author":"x","text":"ignored","created_at":"2026-05-28T00:02:00Z"}
            ]),
        )
        .await;
        let trig = SocialApiOwnPostCommentTrigger::new(client(&server), Arc::clone(&store), 10);
        let cancel = CancellationToken::new();
        let first = trig.next_work_items(&cancel).await.unwrap();
        // c3 is on an unwatched post → filtered out; only c1, c2 surface.
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].kind, work_item_kind::OWN_POST_COMMENT);
        assert_eq!(first[0].platform, PLATFORM);
        // Second poll → all already in socialapi_seen_comments → empty.
        let second = trig.next_work_items(&cancel).await.unwrap();
        assert!(second.is_empty());
    }

    /// #249 fast-path: a webhook-delivered comment on a WATCHED post drains
    /// into one `own_post_comment` WorkItem; a comment on an UN-watched post is
    /// marked processed but produces no work; a later poll of the same comment
    /// (shared `socialapi_seen_comments` ledger) does not duplicate.
    #[tokio::test]
    async fn drains_webhook_comment_event_for_watched_post_only() {
        let (store, _f) = tmp_store();
        let now = now_millis();
        store
            .upsert_own_post(PLATFORM, "post_1", now, now + 86_400_000)
            .unwrap();
        // Watched-post comment → should surface.
        store
            .insert_socialapi_webhook_event(
                "socialapi:comment:post_1:c1",
                "comment",
                None,
                &serde_json::json!({
                    "type":"comment","id":"c1","post_id":"post_1",
                    "author":"jane","text":"nice!","created_at":"2026-05-28T00:00:00Z"
                })
                .to_string(),
            )
            .unwrap();
        // Un-watched-post comment → drained-but-dropped (no work item).
        store
            .insert_socialapi_webhook_event(
                "socialapi:comment:other:c2",
                "comment",
                None,
                &serde_json::json!({
                    "type":"comment","id":"c2","post_id":"other_post",
                    "author":"bob","text":"ignored","created_at":"2026-05-28T00:01:00Z"
                })
                .to_string(),
            )
            .unwrap();

        // Poll re-returns c1 → deduped via the seen-ledger written by the drain.
        let server = MockServer::start().await;
        mount_comments(
            &server,
            serde_json::json!([
                {"id":"c1","post_id":"post_1","author":"jane","text":"nice!","created_at":"2026-05-28T00:00:00Z"}
            ]),
        )
        .await;
        let trig = SocialApiOwnPostCommentTrigger::new(client(&server), Arc::clone(&store), 10);
        let cancel = CancellationToken::new();
        let first = trig.next_work_items(&cancel).await.unwrap();
        assert_eq!(first.len(), 1, "only the watched-post webhook comment surfaces");
        assert_eq!(first[0].external_id, "c1");
        // Both webhook events are now processed; poll re-sees c1 deduped → empty.
        let second = trig.next_work_items(&cancel).await.unwrap();
        assert!(second.is_empty(), "no duplicate from webhook+poll convergence");
    }

    #[tokio::test]
    async fn reply_decision_posts_approval_card() {
        let (store, _f) = tmp_store();
        let now = now_millis();
        store
            .upsert_own_post(PLATFORM, "post_1", now, now + 86_400_000)
            .unwrap();
        let server = MockServer::start().await;
        mount_comments(
            &server,
            serde_json::json!([
                {"id":"c1","post_id":"post_1","author":"jane","text":"Congrats on shipping!","created_at":"2026-05-28T00:00:00Z"}
            ]),
        )
        .await;
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"thoughtful comment"}"#,
            "Thanks so much, Jane!",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let eng = engagement(
            Arc::clone(&store),
            client(&server),
            reasoner,
            Arc::clone(&broker),
            false,
        );
        let n = eng.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn skip_decision_posts_no_card() {
        let (store, _f) = tmp_store();
        let now = now_millis();
        store
            .upsert_own_post(PLATFORM, "post_1", now, now + 86_400_000)
            .unwrap();
        let server = MockServer::start().await;
        mount_comments(
            &server,
            serde_json::json!([
                {"id":"spam","post_id":"post_1","author":"x","text":"🔥🔥🔥","created_at":"2026-05-28T00:00:00Z"}
            ]),
        )
        .await;
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"emoji-only spam"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let eng = engagement(
            Arc::clone(&store),
            client(&server),
            reasoner,
            Arc::clone(&broker),
            false,
        );
        let n = eng.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 0);
        assert!(broker.posts.lock().unwrap().is_empty());
    }

    /// #527: pushed comment events used to strand at `processed = 0` forever
    /// when nothing was in the poll window, because the early return sat ahead
    /// of the drain that is the only thing marking them processed.
    #[tokio::test]
    async fn drain_runs_even_with_no_watched_posts() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        store
            .insert_socialapi_webhook_event(
                "socialapi:comment:post_1:cmt_1",
                "comment",
                Some("acc_1"),
                &serde_json::json!({
                    "type": "comment", "id": "cmt_1", "post_id": "post_1",
                    "author": "jane", "text": "nice",
                    "created_at": "2026-05-28T00:00:00Z"
                })
                .to_string(),
            )
            .unwrap();

        let trigger = SocialApiOwnPostCommentTrigger::new(client(&server), Arc::clone(&store), 10);
        let items = trigger
            .next_work_items(&CancellationToken::new())
            .await
            .unwrap();

        // Nothing watched, so the comment yields no work...
        assert!(items.is_empty());
        // ...but it MUST have been marked processed rather than stranded.
        assert!(
            store
                .take_unprocessed_socialapi_webhook_events("comment", 10)
                .unwrap()
                .is_empty(),
            "comment webhook event stranded at processed=0"
        );
    }

    /// A pushed comment on a watched post still becomes a work item via the
    /// drain, without waiting for the 30-minute poll.
    #[tokio::test]
    async fn drain_emits_for_watched_post() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        store
            .upsert_own_post(PLATFORM, "post_1", now_millis(), now_millis() + 86_400_000)
            .unwrap();
        store
            .insert_socialapi_webhook_event(
                "socialapi:comment:post_1:cmt_2",
                "comment",
                Some("acc_1"),
                &serde_json::json!({
                    "type": "comment", "id": "cmt_2", "post_id": "post_1",
                    "author": "jane", "text": "how did you build it?",
                    "created_at": "2026-05-28T00:00:00Z"
                })
                .to_string(),
            )
            .unwrap();
        Mock::given(method("GET"))
            .and(path("/inbox/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let trigger = SocialApiOwnPostCommentTrigger::new(client(&server), Arc::clone(&store), 10);
        let items = trigger
            .next_work_items(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "cmt_2");
        assert_eq!(items[0].kind, work_item_kind::OWN_POST_COMMENT);
    }
}
