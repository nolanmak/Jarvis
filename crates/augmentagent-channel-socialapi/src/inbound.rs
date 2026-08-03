//! Inbound DM channel for SocialAPI.ai (#242).
//!
//! [`SocialApiDmSource`] is an [`InboundSource`] that, on each `fetch_new`,
//! lists DM conversations across the active SocialAPI.ai accounts via
//! [`SocialApiClient::list_conversations`], walks each conversation's messages,
//! keeps only genuinely *new inbound* messages (the other party's, never our
//! own outbound), diffs them against the store's `socialapi_seen_dms` ledger,
//! and yields one `WorkItem { platform:"socialapi", kind:"dm" }` per fresh
//! inbound message.
//!
//! [`SocialApiDmChannel`] is the [`WorkItemHandler`] that consumes those work
//! items: it deserializes the payload, runs triage → draft, and posts a Discord
//! approval card via [`ApprovalBroker`]. It stops at the approval card by
//! design; the send happens when the operator approves, in the CLI's
//! `approve_socialapi` (#244, merged). Every reply requires Discord approval;
//! nothing here auto-replies.
//!
//! Wrap [`SocialApiDmSource`] in an
//! [`InboundMessageTrigger`](augmentagent_channel_core::trigger::InboundMessageTrigger)
//! and drive it through a
//! [`ChannelRunner`](augmentagent_channel_core::trigger::ChannelRunner) with
//! [`SocialApiDmChannel`] as the handler — the same shape Gmail/LinkedIn use.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::decision::{parse as parse_decision, DecisionKind};
use augmentagent_channel_core::governor::{
    ActionKind, ActionRequest, Denial, Platform, RateGovernor, Risk,
};
use augmentagent_channel_core::prompt::{draft_user_message, triage_user_message};
use augmentagent_channel_core::reasoner::{draft_opts, triage_opts};
use augmentagent_channel_core::trigger::kind as work_item_kind;
use augmentagent_channel_core::trigger::{InboundSource, WorkItem, WorkItemHandler};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{ActionStatus, Email, Store, TriageResult};

use crate::client::SocialApiClient;
use crate::types::{Conversation, DmMessage};
use crate::PLATFORM;

/// Default DM poll cadence (5 min) — DMs are reactive, so a tighter cadence
/// than the own-post comment poller (30 min). The CLI overrides via
/// `AUGMENTAGENT_SOCIALAPI_DM_POLL_SECS`.
pub const DEFAULT_DM_POLL_SECS: u64 = 5 * 60;

/// Default per-tick cap on emitted DM work items. A cheap pre-filter so a flood
/// of inbound DMs can't enqueue hundreds of LLM calls in a single tick.
pub const DEFAULT_DM_MAX_PER_TICK: u32 = 25;

/// Normalized inbound-DM webhook event body (#249) as persisted by the Express
/// receiver into `socialapi_webhook_events.payload_json`. Mirrors the receiver's
/// `normalizeSocialApiEvent` DM shape; everything the drain needs to rebuild a
/// [`SocialApiDmPayload`] without another API call.
#[derive(Debug, Clone, serde::Deserialize)]
struct DmWebhookPayload {
    /// Platform-native message id (dedup key against `socialapi_seen_dms`).
    id: String,
    conversation_id: String,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    with: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    created_at: String,
}

impl From<DmWebhookPayload> for SocialApiDmPayload {
    fn from(w: DmWebhookPayload) -> Self {
        // `with` is the other party. Falling back to `author` here is a
        // DISPLAY convenience only — it makes the approval card read sanely
        // when the push carried no counterparty. It must never be used to
        // decide direction: the fallback makes `author == with` trivially
        // true, which is exactly how our own outbound DMs used to be drafted
        // as inbound (#526). Ownership is decided against the registered
        // account handles in `is_own_handle`.
        let with = if w.with.is_empty() {
            w.author.clone()
        } else {
            w.with
        };
        SocialApiDmPayload {
            conversation_id: w.conversation_id,
            account_id: w.account_id,
            with,
            message_id: w.id,
            author: w.author,
            text: w.text,
            created_at: w.created_at,
        }
    }
}

/// Serialized payload carried in `WorkItem.payload` for an inbound DM. Captures
/// the conversation it arrived in plus the single message that's new.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SocialApiDmPayload {
    /// Conversation/thread id this message belongs to. Carried as the
    /// `thread_id` so a later reply (#244) targets the right thread.
    pub conversation_id: String,
    /// Account this conversation belongs to (the SocialAPI.ai account id).
    pub account_id: String,
    /// Other party's handle / display name on the conversation.
    pub with: String,
    /// Platform-native message id (dedup key + `Email.message_id`).
    pub message_id: String,
    /// Author of the inbound message (the other party).
    pub author: String,
    pub text: String,
    pub created_at: String,
}

impl SocialApiDmPayload {
    fn new(conv: &Conversation, msg: &DmMessage) -> Self {
        Self {
            conversation_id: conv.id.clone(),
            account_id: conv.account_id.clone(),
            with: conv.with.clone(),
            message_id: msg.id.clone(),
            author: msg.author.clone(),
            text: msg.text.clone(),
            created_at: msg.created_at.clone(),
        }
    }

    /// Convert to the store's generic `Email` so the DM rides the same triage →
    /// draft → approval-card path as every other channel. `kind` is stamped
    /// `dm`; `thread_id` carries the conversation id so a later reply targets
    /// the right thread.
    fn into_email(self) -> Email {
        let from = format!("{} <socialapi:{}>", self.with, self.author);
        let subject = format!("[DM from {}]", self.with);
        Email {
            message_id: self.message_id,
            thread_id: Some(self.conversation_id),
            from,
            subject,
            body: self.text,
            date: self.created_at,
            account_entity_id: Some(self.account_id),
            platform: PLATFORM.to_string(),
            kind: work_item_kind::DM.to_string(),
        }
    }
}

/// True iff `msg` is an inbound message (the other party's), not our own
/// outbound reply. SocialAPI.ai normalises the author as the sender's handle;
/// the conversation's `with` field is the other party. We treat a message as
/// inbound when its author matches `with` (case-insensitive, leading `@`
/// tolerated). This is conservative: anything we can't positively attribute to
/// the other party is skipped so we never draft a "reply to ourselves".
fn is_inbound(conv: &Conversation, msg: &DmMessage) -> bool {
    norm_handle(&msg.author) == norm_handle(&conv.with)
}

/// Canonical handle form for identity comparisons: trimmed, no leading `@`,
/// lowercased. Shared by [`is_inbound`] and [`is_own_handle`] so the poll and
/// webhook paths agree on what counts as the same account.
fn norm_handle(s: &str) -> String {
    s.trim().trim_start_matches('@').to_ascii_lowercase()
}

/// True iff `author` is one of OUR registered SocialAPI.ai account handles —
/// i.e. the message is our own outbound, not something to draft a reply to.
///
/// This is the ownership signal for the #249 webhook fast path, which has no
/// conversation to compare against. `handles` comes from
/// `Store::socialapi_account_handles` and is already normalized; `author` is
/// normalized here. An empty `handles` (no accounts synced yet) means we
/// cannot attribute anything, so nothing is filtered — the poll path's
/// `is_inbound` remains the backstop.
fn is_own_handle(handles: &[String], author: &str) -> bool {
    let a = norm_handle(author);
    !a.is_empty() && handles.iter().any(|h| h == &a)
}

/// Polls SocialAPI.ai DM conversations and yields a `dm` WorkItem per genuinely
/// new inbound message. Dedup is durable via `socialapi_seen_dms`.
pub struct SocialApiDmSource {
    client: Arc<SocialApiClient>,
    store: Arc<Store>,
    /// Per-tick cap on emitted work items — a cheap pre-filter so a flood of
    /// DMs can't enqueue hundreds of LLM calls in a single tick.
    max_per_tick: u32,
}

impl SocialApiDmSource {
    pub fn new(client: Arc<SocialApiClient>, store: Arc<Store>, max_per_tick: u32) -> Self {
        Self {
            client,
            store,
            max_per_tick: max_per_tick.max(1),
        }
    }

    /// Fast-path drain of webhook-delivered DM events (#249). Reads up to
    /// `budget` unprocessed `socialapi_webhook_events` of kind `dm`, marks each
    /// processed, and — for the ones not already in `socialapi_seen_dms` —
    /// emits a `dm` WorkItem. Reusing the same dedup ledger as the poll path
    /// means a webhook-delivered DM and a later poll of the same DM collapse to
    /// a single draft. Returns the emitted work items; the API poll runs after
    /// this with whatever budget remains. Best-effort: a malformed row is
    /// marked processed (so it doesn't wedge the queue) and skipped.
    fn drain_webhook_events(&self, budget: u32) -> anyhow::Result<Vec<WorkItem>> {
        if budget == 0 {
            return Ok(Vec::new());
        }
        let events = self
            .store
            .take_unprocessed_socialapi_webhook_events("dm", budget)?;
        // #526: the pushed event carries no conversation to compare against,
        // so `is_inbound` is useless here (and the receiver's `with`-from-
        // `author` display fallback would make it trivially true anyway).
        // Attribute against our own registered handles instead. Read once per
        // drain, not per event.
        let own_handles = self.store.socialapi_account_handles().unwrap_or_else(|e| {
            warn!("socialapi dm webhook: account handle lookup failed: {e}");
            Vec::new()
        });
        let mut out = Vec::new();
        for ev in events {
            // Always mark processed first so a poison row can't be re-drained
            // forever; the seen-ledger is the authoritative dedup if the same
            // id later arrives via poll.
            if let Err(e) = self.store.mark_socialapi_webhook_event_processed(&ev.id) {
                warn!(event = %ev.id, "socialapi dm webhook: mark processed failed: {e}");
            }
            let wp: DmWebhookPayload = match serde_json::from_str(&ev.payload_json) {
                Ok(p) => p,
                Err(e) => {
                    warn!(event = %ev.id, "socialapi dm webhook: payload decode failed: {e}");
                    continue;
                }
            };
            let payload: SocialApiDmPayload = wp.into();
            // Our own outbound message echoed back as a push — never draft a
            // reply to ourselves. Marked processed above, so it drops here.
            if is_own_handle(&own_handles, &payload.author) {
                debug!(
                    event = %ev.id,
                    author = %payload.author,
                    "socialapi dm webhook: skipping our own outbound message"
                );
                continue;
            }
            // Durable one-shot dedup keyed on (conversation_id, message_id) —
            // the SAME ledger the poll path writes, so no double-draft.
            let is_new = self.store.record_seen_socialapi_dm(
                &payload.conversation_id,
                &payload.message_id,
                Some(payload.author.as_str()),
                Some(payload.text.as_str()),
            )?;
            if !is_new {
                continue;
            }
            out.push(WorkItem {
                platform: PLATFORM.into(),
                kind: work_item_kind::DM.into(),
                external_id: payload.message_id.clone(),
                payload: serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null),
            });
        }
        if !out.is_empty() {
            info!(n = out.len(), "socialapi dm source: drained webhook events (fast-path)");
        }
        Ok(out)
    }
}

#[async_trait]
impl InboundSource for SocialApiDmSource {
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
        let mut budget = self.max_per_tick;
        // #249 fast-path: drain webhook-delivered DM events AHEAD of the API
        // poll so near-real-time pushes don't wait for the next tick. Shares
        // the `socialapi_seen_dms` dedup ledger with the poll below, so a
        // webhook item drained here is skipped when the poll later sees it.
        let mut out = self.drain_webhook_events(budget)?;
        budget = budget.saturating_sub(out.len() as u32);

        // Same ownership backstop the drain uses (#526). `is_inbound` below is
        // the primary direction check on this path; this catches the case
        // where the API reports us as the conversation counterparty.
        let own_handles = self.store.socialapi_account_handles().unwrap_or_default();
        let accounts = self.store.active_socialapi_account_ids()?;
        // No registered accounts → poll the whole inbox once (account_id=None),
        // same fallback the own-post comment poller uses.
        let scopes: Vec<Option<String>> = if accounts.is_empty() {
            vec![None]
        } else {
            accounts.into_iter().map(Some).collect()
        };

        for scope in scopes {
            if budget == 0 {
                break;
            }
            let conversations = match self.client.list_conversations(scope.as_deref()).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(account = ?scope, error = %e, "dm conversation list failed; skipping account");
                    continue;
                }
            };
            for conv in conversations {
                // #244 supersede: if the user has manually replied in this
                // thread (an outbound message we didn't send via Approve),
                // flip any still-pending socialapi draft on the conversation to
                // `superseded` so a stale card never re-sends what the user
                // already answered. Mirrors the email outbound observer. Keyed
                // on the conversation id, which is the draft's `thread_id`.
                // Best-effort: a store error here must not block fetching new
                // inbound work.
                let user_replied = conv.messages.iter().any(|m| !is_inbound(&conv, m));
                if user_replied {
                    match self.store.mark_pending_drafts_superseded_by_thread(
                        &conv.id,
                        "superseded by manual reply",
                    ) {
                        Ok(ids) if !ids.is_empty() => {
                            info!(
                                conversation = %conv.id,
                                superseded = ids.len(),
                                "socialapi dm source: superseded pending drafts after manual reply"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!(conversation = %conv.id, "socialapi dm source: supersede failed: {e}");
                        }
                    }
                }
                for msg in &conv.messages {
                    if budget == 0 {
                        break;
                    }
                    if !is_inbound(&conv, msg) || is_own_handle(&own_handles, &msg.author) {
                        continue;
                    }
                    // Durable one-shot dedup keyed on (conversation_id, message_id).
                    let is_new = self.store.record_seen_socialapi_dm(
                        &conv.id,
                        &msg.id,
                        Some(msg.author.as_str()),
                        Some(msg.text.as_str()),
                    )?;
                    if !is_new {
                        continue;
                    }
                    out.push(to_work_item(&conv, msg));
                    budget -= 1;
                }
            }
        }
        debug!(n = out.len(), "socialapi dm source: new inbound messages");
        Ok(out)
    }
}

fn to_work_item(conv: &Conversation, msg: &DmMessage) -> WorkItem {
    let payload = SocialApiDmPayload::new(conv, msg);
    WorkItem {
        platform: PLATFORM.into(),
        kind: work_item_kind::DM.into(),
        external_id: msg.id.clone(),
        payload: serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
    }
}

/// Config for the SocialAPI.ai DM channel. Mirrors the subset of the own-post
/// engagement config the handler actually reads.
#[derive(Debug, Clone)]
pub struct SocialApiDmConfig {
    /// When true, drafts are logged (and the governor permit rolled back) but
    /// no approval card is posted.
    pub dry_run: bool,
    /// Wiki root passed into the triage/draft reasoner opts (grounding).
    pub wiki_root: Option<PathBuf>,
    /// Skill dir whose `SKILL.md` seeds the draft system prompt.
    pub skill_dir: PathBuf,
}

impl Default for SocialApiDmConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            wiki_root: None,
            skill_dir: PathBuf::from("skills/email-triage"),
        }
    }
}

/// [`WorkItemHandler`] for inbound SocialAPI.ai DMs. Deserializes the payload,
/// runs triage → draft, and posts a Discord approval card. Stops at the
/// approval card — the reply send happens on approve (#244). Wrapped in the merged
/// RateGovernor `Dm` permit/record envelope (no-op fallthrough until SocialAPI
/// has `Platform` rate-table rows, exactly like the own-post handler).
pub struct SocialApiDmChannel<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    pub governor: Arc<dyn RateGovernor>,
    pub config: SocialApiDmConfig,
}

impl<R: Reasoner + 'static> SocialApiDmChannel<R> {
    /// Triage + draft + (unless dry-run) approval-card one inbound DM. Returns
    /// `true` when an approval card was posted.
    pub async fn handle_dm(&self, payload: SocialApiDmPayload) -> anyhow::Result<bool> {
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
                error!(message_id = %email.message_id, "dm triage parse failed: {e}; raw={raw}");
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
            // Spam / not-worth-a-reply → record + skip. Triage is the filter.
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

        // Governor preflight. SocialAPI.ai has no `Platform` rate-table rows yet
        // (`Platform::parse("socialapi")` is None), so this falls through to "no
        // permit, proceed" — the approval card is the gate.
        let permit = if let Some(plat) = Platform::parse(PLATFORM) {
            let req = ActionRequest {
                platform: plat,
                action: ActionKind::Dm,
                account_id: format!(
                    "socialapi:{}",
                    email.account_entity_id.clone().unwrap_or_default()
                ),
                risk: Risk::Low,
                cause: format!("dm:{}", email.message_id),
                target_id: Some(email.message_id.clone()),
                target_attrs: None,
            };
            match self.governor.permit(req).await {
                Ok(p) => Some(p),
                Err(Denial::ApprovalRequired { .. }) => None,
                Err(d) => {
                    info!(dm = %email.message_id, "socialapi dm reply deferred by governor: {d}");
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
                "[socialapi dm reply dry-run] {}\n--- reply ---\n{}\n--- /reply ---",
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
        if let Err(e) = self.approvals.post_approval(&action_id, &email, &draft).await {
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
        if let Some(p) = permit {
            let _ = self
                .governor
                .record(p, augmentagent_channel_core::governor::Outcome::Ok)
                .await;
        }
        info!(action_id, dm = %email.message_id, "socialapi dm reply card posted");
        Ok(true)
    }
}

#[async_trait]
impl<R: Reasoner + 'static> WorkItemHandler for SocialApiDmChannel<R> {
    async fn handle(&self, item: WorkItem) -> anyhow::Result<()> {
        let payload: SocialApiDmPayload = serde_json::from_value(item.payload.clone())
            .map_err(|e| anyhow::anyhow!("socialapi dm payload decode failed: {e}"))?;
        self.handle_dm(payload).await?;
        Ok(())
    }
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

    async fn mount_conversations(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/inbox/conversations"))
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
        async fn record_halt(&self, _: Platform, _: HaltReason, _: i64) -> anyhow::Result<()> {
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

    fn channel(
        store: Arc<Store>,
        reasoner: Arc<ScriptedReasoner>,
        broker: Arc<RecordingBroker>,
        dry_run: bool,
    ) -> SocialApiDmChannel<ScriptedReasoner> {
        SocialApiDmChannel {
            store,
            reasoner,
            approvals: broker,
            governor: Arc::new(AlwaysPermit),
            config: SocialApiDmConfig {
                dry_run,
                wiki_root: None,
                skill_dir: PathBuf::from("/tmp/nonexistent-skill"),
            },
        }
    }

    #[test]
    fn is_inbound_matches_other_party_only() {
        let conv = Conversation {
            id: "c1".into(),
            account_id: "acc_1".into(),
            with: "jane".into(),
            messages: vec![],
        };
        let from_jane = DmMessage {
            id: "m1".into(),
            author: "@Jane".into(),
            text: "hi".into(),
            created_at: "t".into(),
        };
        let from_me = DmMessage {
            id: "m2".into(),
            author: "me".into(),
            text: "reply".into(),
            created_at: "t".into(),
        };
        assert!(is_inbound(&conv, &from_jane));
        assert!(!is_inbound(&conv, &from_me));
    }

    #[tokio::test]
    async fn source_yields_new_inbound_messages_once_then_dedups() {
        let (store, _f) = tmp_store();
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!([
                {
                    "id": "conv_1", "account_id": "acc_1", "with": "jane",
                    "messages": [
                        {"id":"m1","author":"jane","text":"hey there","created_at":"2026-05-28T00:00:00Z"},
                        {"id":"m2","author":"me","text":"our outbound","created_at":"2026-05-28T00:01:00Z"},
                        {"id":"m3","author":"jane","text":"you around?","created_at":"2026-05-28T00:02:00Z"}
                    ]
                }
            ]),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);
        // Only the two inbound (jane) messages surface; our own outbound (me) is
        // filtered out.
        let first = source.fetch_new().await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].kind, work_item_kind::DM);
        assert_eq!(first[0].platform, PLATFORM);
        assert_eq!(first[0].external_id, "m1");
        // Second poll → all already in socialapi_seen_dms → empty.
        let second = source.fetch_new().await.unwrap();
        assert!(second.is_empty());
    }

    /// #244 supersede: a manual (outbound) reply in a conversation flips any
    /// still-pending socialapi draft on that thread to `superseded` on the
    /// next source poll, mirroring the email outbound observer.
    #[tokio::test]
    async fn source_supersedes_pending_draft_on_manual_reply() {
        let (store, _f) = tmp_store();
        // Seed a pending socialapi DM draft on conversation conv_1.
        let email = SocialApiDmPayload {
            conversation_id: "conv_1".into(),
            account_id: "acc_1".into(),
            with: "jane".into(),
            message_id: "m1".into(),
            author: "jane".into(),
            text: "hey".into(),
            created_at: "2026-05-28T00:00:00Z".into(),
        }
        .into_email();
        store.upsert_email(&email).unwrap();
        let action_id = store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("a pending draft reply"),
                ActionStatus::Pending,
            )
            .unwrap();

        // Conversation now shows our own outbound message → the user replied.
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!([
                {
                    "id": "conv_1", "account_id": "acc_1", "with": "jane",
                    "messages": [
                        {"id":"m1","author":"jane","text":"hey","created_at":"2026-05-28T00:00:00Z"},
                        {"id":"m2","author":"me","text":"manual reply","created_at":"2026-05-28T00:05:00Z"}
                    ]
                }
            ]),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);
        let _ = source.fetch_new().await.unwrap();

        let row = store.get_action_with_email(&action_id).unwrap().unwrap();
        assert_eq!(row.action.status, "superseded");
    }

    /// #249 fast-path: a webhook-delivered DM event drained from
    /// `socialapi_webhook_events` surfaces as a `dm` WorkItem exactly once, is
    /// marked processed, and a later API poll of the SAME message id (shared
    /// `socialapi_seen_dms` ledger) produces no duplicate.
    #[tokio::test]
    async fn drains_webhook_dm_event_once_and_dedups_against_poll() {
        let (store, _f) = tmp_store();
        // Seed a webhook event for an inbound DM from jane.
        let payload = serde_json::json!({
            "type": "dm",
            "id": "m1",
            "conversation_id": "conv_1",
            "account_id": "acc_1",
            "with": "jane",
            "author": "jane",
            "text": "hey there",
            "created_at": "2026-05-28T00:00:00Z"
        });
        let new = store
            .insert_socialapi_webhook_event(
                "socialapi:dm:conv_1:m1",
                "dm",
                Some("acc_1"),
                &payload.to_string(),
            )
            .unwrap();
        assert!(new);

        // The API poll returns the SAME message (m1) plus our own outbound.
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!([
                {
                    "id": "conv_1", "account_id": "acc_1", "with": "jane",
                    "messages": [
                        {"id":"m1","author":"jane","text":"hey there","created_at":"2026-05-28T00:00:00Z"}
                    ]
                }
            ]),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);

        // First fetch: drains the webhook event → 1 work item (the poll sees m1
        // but it's already in seen_dms from the drain, so no double).
        let first = source.fetch_new().await.unwrap();
        assert_eq!(first.len(), 1, "webhook drain should surface m1 exactly once");
        assert_eq!(first[0].external_id, "m1");
        assert_eq!(first[0].kind, work_item_kind::DM);

        // Webhook event is now processed → second fetch drains nothing and the
        // poll re-sees m1 but it's deduped → empty.
        let second = source.fetch_new().await.unwrap();
        assert!(second.is_empty(), "no duplicate from webhook+poll convergence");
    }

    #[tokio::test]
    async fn reply_decision_posts_approval_card() {
        let (store, _f) = tmp_store();
        let payload = SocialApiDmPayload {
            conversation_id: "conv_1".into(),
            account_id: "acc_1".into(),
            with: "jane".into(),
            message_id: "m1".into(),
            author: "jane".into(),
            text: "Can we hop on a call this week?".into(),
            created_at: "2026-05-28T00:00:00Z".into(),
        };
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"genuine question"}"#,
            "Sure, how about Thursday?",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = channel(Arc::clone(&store), reasoner, Arc::clone(&broker), false);
        let posted = ch.handle_dm(payload).await.unwrap();
        assert!(posted);
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn skip_decision_posts_no_card() {
        let (store, _f) = tmp_store();
        let payload = SocialApiDmPayload {
            conversation_id: "conv_1".into(),
            account_id: "acc_1".into(),
            with: "spammer".into(),
            message_id: "spam1".into(),
            author: "spammer".into(),
            text: "🔥 buy now 🔥".into(),
            created_at: "2026-05-28T00:00:00Z".into(),
        };
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"skip","reason":"spam"}"#,
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = channel(Arc::clone(&store), reasoner, Arc::clone(&broker), false);
        let posted = ch.handle_dm(payload).await.unwrap();
        assert!(!posted);
        assert!(broker.posts.lock().unwrap().is_empty());
    }

    fn seed_account(store: &Store, id: &str, handle: &str) {
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO socialapi_accounts \
                        (id, platform, account_handle, active, created_at_ms, updated_at_ms) \
                     VALUES (?1,'instagram',?2,1,0,0)",
                    [id, handle],
                )
            })
            .unwrap();
    }

    fn seed_dm_event(store: &Store, ev_id: &str, payload: serde_json::Value) {
        store
            .insert_socialapi_webhook_event(ev_id, "dm", Some("acc_1"), &payload.to_string())
            .unwrap();
    }

    #[test]
    fn is_own_handle_matches_case_and_at_prefix() {
        let handles = vec!["acme".to_string()];
        assert!(is_own_handle(&handles, "@Acme"));
        assert!(is_own_handle(&handles, " acme "));
        assert!(!is_own_handle(&handles, "someone_else"));
        // No registered handles ⇒ nothing is attributable to us.
        assert!(!is_own_handle(&[], "acme"));
        // An empty author must never match an empty-ish handle set.
        assert!(!is_own_handle(&handles, "   "));
    }

    /// #526: a pushed event for OUR OWN outbound DM must not become a work
    /// item. The receiver's `with`-from-`author` fallback made `is_inbound`
    /// trivially true, so this used to be drafted as though someone had
    /// messaged us.
    #[tokio::test]
    async fn drain_skips_our_own_outbound_dm() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        seed_account(&store, "acc_1", "acme");
        seed_dm_event(
            &store,
            "socialapi:dm:conv_1:msg_1",
            serde_json::json!({
                "type": "dm", "id": "msg_1", "conversation_id": "conv_1",
                "account_id": "acc_1", "with": "", "author": "@Acme",
                "text": "thanks for reaching out", "created_at": "2026-05-28T00:00:00Z"
            }),
        );
        mount_conversations(&server, serde_json::json!([])).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        let items = src.fetch_new().await.unwrap();
        assert!(items.is_empty(), "own outbound DM must not be drafted");
        // Still marked processed, so it can't wedge the drain queue.
        assert!(store
            .take_unprocessed_socialapi_webhook_events("dm", 10)
            .unwrap()
            .is_empty());
    }

    /// The counterpart: a genuine inbound push from someone else still lands.
    #[tokio::test]
    async fn drain_emits_genuine_inbound_dm() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        seed_account(&store, "acc_1", "acme");
        seed_dm_event(
            &store,
            "socialapi:dm:conv_1:msg_2",
            serde_json::json!({
                "type": "dm", "id": "msg_2", "conversation_id": "conv_1",
                "account_id": "acc_1", "with": "jane", "author": "jane",
                "text": "hey, quick question", "created_at": "2026-05-28T00:00:00Z"
            }),
        );
        mount_conversations(&server, serde_json::json!([])).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        let items = src.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "msg_2");
        assert_eq!(items[0].kind, work_item_kind::DM);
    }

    /// With no accounts synced we cannot attribute anything, so the drain must
    /// stay permissive rather than silently swallowing every push.
    #[tokio::test]
    async fn drain_without_registered_handles_still_emits() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        seed_dm_event(
            &store,
            "socialapi:dm:conv_9:msg_9",
            serde_json::json!({
                "type": "dm", "id": "msg_9", "conversation_id": "conv_9",
                "account_id": "acc_9", "with": "", "author": "anyone",
                "text": "hello", "created_at": "2026-05-28T00:00:00Z"
            }),
        );
        mount_conversations(&server, serde_json::json!([])).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        assert_eq!(src.fetch_new().await.unwrap().len(), 1);
    }
}
