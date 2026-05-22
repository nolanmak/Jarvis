//! LinkedIn connection-request triage (#58.4).
//!
//! [`InvitationsTrigger`] polls the Voyager `relationships/invitationViews`
//! endpoint, records each pending invite into the durable
//! `connection_requests` table (idempotent on `(platform, external_id)`), and
//! yields one `WorkItem { kind:"connection_request" }` per genuinely new
//! invite.
//!
//! [`ConnectionRequestEngagement`] runs each through triage. The triage
//! decision space here is **accept vs ignore** (the #58 spec's
//! accept/decline/accept_and_dm/note_only collapses to a binary for v1: a
//! Reply decision ⇒ recommend *accept* and surface a 1-line opener draft; any
//! other ⇒ recommend *ignore*). The recommendation is surfaced as an approval
//! card — the user makes the call; nothing is auto-accepted. The
//! accept/ignore wire call is the approver's job (`act_on_invitation`) on the
//! button click, RateGovernor `ConnectionInvite`-gated there.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::{Trigger, WorkItem};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Store, TriageResult};

use crate::api::LinkedInApi;
use crate::channel::LinkedInChannelConfig;
use crate::types::Invitation;

/// Default invitation poll cadence: 4h (same posture as the DM poll —
/// invites are low-velocity; no need to hammer Voyager).
pub const DEFAULT_INVITATION_POLL_SECS: u64 = 4 * 60 * 60;

/// Serialized payload carried in `WorkItem.payload`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ConnectionRequestPayload {
    pub invitation_urn: String,
    pub requester_name: String,
    pub requester_url: String,
    pub headline: String,
    pub message: String,
    pub created_at_ms: i64,
}

/// Polls pending invitations and yields `connection_request` work items.
pub struct InvitationsTrigger<L: LinkedInApi> {
    api: Arc<L>,
    store: Arc<Store>,
}

impl<L: LinkedInApi> InvitationsTrigger<L> {
    pub fn new(api: Arc<L>, store: Arc<Store>) -> Self {
        Self { api, store }
    }
}

#[async_trait]
impl<L: LinkedInApi + 'static> Trigger for InvitationsTrigger<L> {
    async fn next_work_items(&self, _cancel: &CancellationToken) -> anyhow::Result<Vec<WorkItem>> {
        let invites = match self.api.fetch_pending_invitations().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "invitation fetch failed");
                return Ok(Vec::new());
            }
        };
        if invites.is_empty() {
            debug!("connection-request poller: no pending invitations");
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for inv in invites {
            // Durable one-shot dedup: record_connection_request returns false
            // if this invite is already queued.
            let is_new = self.store.record_connection_request(
                "linkedin",
                &inv.invitation_urn,
                Some(inv.requester_name.as_str()),
                Some(inv.requester_url.as_str()),
                if inv.message.trim().is_empty() {
                    None
                } else {
                    Some(inv.message.as_str())
                },
            )?;
            if !is_new {
                continue;
            }
            out.push(to_work_item(&inv));
        }
        Ok(out)
    }
}

fn to_work_item(inv: &Invitation) -> WorkItem {
    let payload = ConnectionRequestPayload {
        invitation_urn: inv.invitation_urn.clone(),
        requester_name: inv.requester_name.clone(),
        requester_url: inv.requester_url.clone(),
        headline: inv.headline.clone(),
        message: inv.message.clone(),
        created_at_ms: inv.created_at_ms,
    };
    WorkItem {
        platform: "linkedin".into(),
        kind: augmentagent_channel_core::work_item_kind::CONNECTION_REQUEST.into(),
        external_id: inv.invitation_urn.clone(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Drives [`InvitationsTrigger`] on a cadence and runs each invite through
/// triage. The triage outcome maps to an accept/ignore recommendation
/// surfaced on an approval card. Nothing is auto-accepted; the approver's
/// button click is the only thing that calls `act_on_invitation`.
pub struct ConnectionRequestEngagement<L: LinkedInApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub trigger: Arc<InvitationsTrigger<L>>,
    pub member_urn: String,
    pub config: LinkedInChannelConfig,
    pub poll_interval: Duration,
}

impl<L: LinkedInApi + 'static, R: Reasoner + 'static> ConnectionRequestEngagement<L, R> {
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            interval_secs = self.poll_interval.as_secs(),
            "connection-request triage started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("connection-request triage: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once(&shutdown).await {
                        Ok(n) => info!(triaged = n, "connection-request poll complete"),
                        Err(e) => error!("connection-request poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One poll: ask the trigger for fresh invites, triage each, post an
    /// accept/ignore approval card. Returns the number of cards posted.
    pub async fn poll_once(&self, cancel: &CancellationToken) -> anyhow::Result<usize> {
        let items = self.trigger.next_work_items(cancel).await?;
        let mut posted = 0usize;
        for item in items {
            let payload: ConnectionRequestPayload =
                match serde_json::from_value(item.payload.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("connection-request payload decode failed: {e}");
                        continue;
                    }
                };
            match self.handle_invite(payload).await {
                Ok(true) => posted += 1,
                Ok(false) => {}
                Err(e) => error!("handle_invite failed: {e:#}"),
            }
        }
        Ok(posted)
    }

    async fn handle_invite(&self, payload: ConnectionRequestPayload) -> anyhow::Result<bool> {
        let inv = Invitation {
            invitation_urn: payload.invitation_urn,
            requester_name: payload.requester_name,
            requester_url: payload.requester_url,
            headline: payload.headline,
            message: payload.message,
            created_at_ms: payload.created_at_ms,
        };
        let email = inv.into_email(&self.member_urn);
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
                error!(message_id = %email.message_id, "invite triage parse failed: {e}; raw={raw}");
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

        // Reply ⇒ recommend ACCEPT (+ a 1-line opener the user can send if
        // they pick accept_and_dm). Anything else ⇒ recommend IGNORE: surface
        // a heads-up notice, no action card (the invite just sits pending in
        // the LI inbox, exactly as before — we never auto-decline).
        if !matches!(decision.decision, DecisionKind::Reply) {
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
            let reason = decision
                .reason
                .as_deref()
                .unwrap_or("low-signal connection request");
            if let Err(e) = self.approvals.post_flag_notice(&email, reason).await {
                warn!(message_id = %email.message_id, "invite flag notice failed: {e}");
            }
            return Ok(false);
        }

        let opener = self
            .reasoner
            .call(
                &draft_opts(String::new(), self.config.wiki_root.clone()),
                &draft_user_message(&email, "", "", "", "", ""),
            )
            .await?
            .trim()
            .to_string();
        // The draft body doubles as the approval card preview + the
        // accept_and_dm opener the approver sends on click.
        let card_body = format!(
            "Recommended: ACCEPT.\n{}\n\nSuggested opener if you accept_and_dm:\n{}",
            decision
                .reason
                .as_deref()
                .unwrap_or("looks like a worthwhile connection"),
            opener
        );

        if self.config.dry_run {
            self.store.log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some(&card_body),
                ActionStatus::DryRun,
            )?;
            self.store
                .mark_email_processed(&email.message_id, TriageResult::Reply)?;
            println!(
                "[linkedin connection-request dry-run] {}\n{}",
                email.subject, card_body
            );
            return Ok(false);
        }

        let action_id = self.store.log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(&card_body),
            ActionStatus::Pending,
        )?;
        if let Err(e) = self
            .approvals
            .post_approval(&action_id, &email, &card_body)
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
        info!(action_id, invite = %email.message_id, "connection-request card posted");
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::LinkedInError;
    use crate::posting::{PostDraft, ShareUrn};
    use std::path::PathBuf;

    use crate::types::{Dm, FeedPost, MemberUrn, PostComment};
    use augmentagent_approval_discord::ApprovalError;
    use augmentagent_channel_core::{Reasoner, ReasonerOpts};
    use augmentagent_store::Email;

    struct StubApi {
        invites: Vec<Invitation>,
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
        async fn fetch_post_comments(&self, _: &str) -> Result<Vec<PostComment>, LinkedInError> {
            Ok(vec![])
        }
        async fn fetch_pending_invitations(&self) -> Result<Vec<Invitation>, LinkedInError> {
            Ok(self.invites.clone())
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
        flags: std::sync::Mutex<Vec<String>>,
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
        async fn post_flag_notice(&self, email: &Email, _: &str) -> Result<(), ApprovalError> {
            self.flags.lock().unwrap().push(email.message_id.clone());
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

    fn invite(urn: &str) -> Invitation {
        Invitation {
            invitation_urn: urn.into(),
            requester_name: "Sam Lee".into(),
            requester_url: "https://www.linkedin.com/in/sam-lee".into(),
            headline: "Founder at Beta".into(),
            message: "Loved your talk".into(),
            created_at_ms: 1_776_630_000_000,
        }
    }

    fn eng(
        store: Arc<Store>,
        api: Arc<StubApi>,
        reasoner: Arc<ScriptedReasoner>,
        broker: Arc<RecordingBroker>,
    ) -> ConnectionRequestEngagement<StubApi, ScriptedReasoner> {
        ConnectionRequestEngagement {
            store: Arc::clone(&store),
            reasoner,
            approvals: broker,
            trigger: Arc::new(InvitationsTrigger::new(api, store)),
            member_urn: "urn:li:fsd_profile:ME".into(),
            config: LinkedInChannelConfig {
                dry_run: false,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
                ..Default::default()
            },
            poll_interval: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn trigger_records_and_dedups_invites() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            invites: vec![invite("urn:li:invitation:1"), invite("urn:li:invitation:2")],
        });
        let trig = InvitationsTrigger::new(api, Arc::clone(&store));
        let cancel = CancellationToken::new();
        let first = trig.next_work_items(&cancel).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            first[0].kind,
            augmentagent_channel_core::work_item_kind::CONNECTION_REQUEST
        );
        // Both queued in connection_requests now → second poll dedups.
        let second = trig.next_work_items(&cancel).await.unwrap();
        assert!(second.is_empty());
        assert_eq!(store.pending_connection_requests().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn accept_recommendation_posts_card() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            invites: vec![invite("urn:li:invitation:1")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"known from the conference circuit"}"#,
            "Great to connect, Sam — enjoyed your work at Beta!",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let e = eng(Arc::clone(&store), api, reasoner, Arc::clone(&broker));
        let n = e.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(broker.flags.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignore_recommendation_posts_notice_only() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            invites: vec![invite("urn:li:invitation:9")],
        });
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"spammy recruiter blast"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let e = eng(Arc::clone(&store), api, reasoner, Arc::clone(&broker));
        let n = e.poll_once(&CancellationToken::new()).await.unwrap();
        assert_eq!(n, 0);
        assert!(broker.posts.lock().unwrap().is_empty());
        assert_eq!(broker.flags.lock().unwrap().len(), 1);
    }
}
