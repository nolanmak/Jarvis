//! Inbound DM channel for SocialAPI.ai (#242).
//!
//! [`SocialApiDmSource`] is an [`InboundSource`] that, on each `fetch_new`,
//! lists DM conversations across the active SocialAPI.ai accounts via
//! [`SocialApiClient::list_conversations`], fetches each thread's messages
//! via [`SocialApiClient::list_messages`] (the list endpoint embeds none —
//! #543), keeps only the *unanswered incoming tail* — provider-stated
//! `direction: incoming` messages newer than our latest outgoing — diffs
//! them against the store's `socialapi_seen_dms` ledger, and yields one
//! `WorkItem { platform:"socialapi", kind:"dm" }` per fresh inbound message.
//!
//! [`SocialApiDmChannel`] is the [`WorkItemHandler`] that consumes those work
//! items: it deserializes the payload, runs triage → draft, and posts a Discord
//! approval card via [`ApprovalBroker`]. It stops at the approval card by
//! design; the send happens when the operator approves, in the CLI's
//! `approve_socialapi` (#244, merged). Every reply requires Discord approval;
//! nothing here auto-replies.
//!
//! #671: the ledger is a *terminal-outcome* marker, written by the handler once
//! a DM has been skipped, carded or dry-run — never by the source when it emits.
//! The source only reads it. A transient reasoner failure therefore leaves the
//! DM unledgered and the next poll re-feeds it, mirroring how gmail leaves a
//! triage-stage error unread with `agentProcessedAt` NULL. Writing on emission
//! turned any provider outage into a permanent silent drop of a human's DM.
//!
//! Wrap [`SocialApiDmSource`] in an
//! [`InboundMessageTrigger`](augmentagent_channel_core::trigger::InboundMessageTrigger)
//! and drive it through a
//! [`ChannelRunner`](augmentagent_channel_core::trigger::ChannelRunner) with
//! [`SocialApiDmChannel`] as the handler — the same shape Gmail/LinkedIn use.

use std::collections::HashSet;
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
use augmentagent_channel_core::reasoner::{socialapi_draft_opts, triage_opts};
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

/// Max conversations whose messages are fetched per account per tick. The
/// conversation list is newest-activity-first and reading a thread costs one
/// extra request per conversation (#543), so this bounds the poll's request
/// fan-out; a thread with fresh activity re-enters the top of the list, so
/// nothing is permanently missed.
pub const DM_CONVERSATIONS_PER_POLL: usize = 10;

/// Ignore unanswered inbound DMs older than this. First-run guard: the seen
/// ledger starts empty, and without a horizon the first poll after connecting
/// an account would draft replies to months-old, long-settled threads. Stale
/// messages are still written to the ledger so they stay permanently skipped —
/// "never draft this" is a terminal decision the *source* is entitled to make.
/// That also caps the #671 re-feed loop: a DM whose triage keeps failing ages
/// out of the horizon after this many days instead of retrying forever.
pub const DM_MAX_AGE_DAYS: i64 = 3;

/// #795 — how many times a DM whose triage/approval errored is re-fed for
/// another attempt before it is flagged for the owner. Each attempt costs a
/// full ~24k-token triage call, so this is the amplification bound (#448).
const DM_MAX_TRIAGE_RETRIES: i64 = 3;

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
    /// Provider-stated direction, normalized by the Express receiver's
    /// `isOutbound`. `true` means the message is ours. Absent (⇒ `false`)
    /// means *unstated*, not *inbound* — see the drain's attribution walk.
    #[serde(default)]
    outbound: bool,
    /// Underlying network the DM arrived on, when the push states it.
    #[serde(default)]
    sub_platform: String,
    /// Shared media on the pushed message (#573).
    #[serde(default)]
    attachment_url: Option<String>,
}

impl From<DmWebhookPayload> for SocialApiDmPayload {
    fn from(w: DmWebhookPayload) -> Self {
        // `with` is the other party. Falling back to `author` here is a
        // DISPLAY convenience only — it makes the approval card read sanely
        // when the push carried no counterparty. It must never be used to
        // decide direction: the fallback makes `author == with` trivially
        // true, which is exactly how our own outbound DMs used to be drafted
        // as inbound (#526). Ownership is decided by the provider's stated
        // direction and the registered account handles, in the drain's
        // attribution walk — never here.
        //
        // `author` is the right display fallback precisely because the drain
        // only ever emits INBOUND messages, so for anything that reaches a
        // card the author IS the counterparty. `recipient`/`to` would be the
        // opposite: on an inbound DM the destination is us.
        let with = if w.with.is_empty() {
            w.author.clone()
        } else {
            w.with
        };
        SocialApiDmPayload {
            sub_platform: w.sub_platform,
            attachment_url: w.attachment_url.filter(|u| !u.trim().is_empty()),
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
    /// Underlying social network this DM actually arrived on ("instagram",
    /// "x", "linkedin", …). One SocialAPI.ai key fronts several connected
    /// accounts, so without this the approval card said only "[DM from jane]"
    /// and there was no way to tell which inbox it came from. Empty when the
    /// API or push didn't say; `platform_label` degrades gracefully.
    #[serde(default)]
    pub sub_platform: String,
    /// Media the sender shared — a Reel, a post, an image (#573).
    ///
    /// The API has always returned this on `DmMessage`; it was parsed on every
    /// poll and then dropped before the payload was built, so a shared Reel
    /// reached the card as an empty message and the model dutifully drafted
    /// "came through without any message". It could not see the thing it was
    /// being asked to reply to.
    #[serde(default)]
    pub attachment_url: Option<String>,
}

/// Human label for a sub-platform, for card titles. Known networks get their
/// conventional casing; anything unrecognized is passed through as-is rather
/// than dropped, so a new network shows up readable instead of invisible.
pub fn platform_label(sub_platform: &str) -> Option<String> {
    let s = sub_platform.trim();
    if s.is_empty() {
        return None;
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "instagram" | "ig" => "Instagram".to_string(),
        "x" | "twitter" => "X".to_string(),
        "linkedin" | "li" => "LinkedIn".to_string(),
        "facebook" | "fb" => "Facebook".to_string(),
        "threads" => "Threads".to_string(),
        "tiktok" => "TikTok".to_string(),
        "youtube" => "YouTube".to_string(),
        "pinterest" => "Pinterest".to_string(),
        _ => s.to_string(),
    })
}

impl SocialApiDmPayload {
    fn new(conv: &Conversation, msg: &DmMessage) -> Self {
        // Display fallback mirrors the webhook receiver's: the poll only ever
        // emits positively-incoming messages, so the sender IS the
        // counterparty when the conversation carries no participant name.
        let with = if conv.participant_name.is_empty() {
            msg.sender_name.clone()
        } else {
            conv.participant_name.clone()
        };
        Self {
            sub_platform: conv.platform.clone(),
            conversation_id: conv.id.clone(),
            account_id: conv.account_id.clone(),
            with,
            message_id: msg.id.clone(),
            author: sender_handle(msg),
            text: msg.text.clone(),
            created_at: msg.created_at.clone(),
            attachment_url: msg.attachment_url.clone().filter(|u| !u.trim().is_empty()),
        }
    }

    /// Convert to the store's generic `Email` so the DM rides the same triage →
    /// draft → approval-card path as every other channel. `kind` is stamped
    /// `dm`; `thread_id` carries the conversation id so a later reply targets
    /// the right thread.
    fn into_email(self) -> Email {
        // Name the network in BOTH the from-line and the subject. The card
        // renders the subject as its title, and the triage/draft prompts see
        // both — the model should know it is writing an Instagram DM rather
        // than a LinkedIn one, since register and length conventions differ.
        let label = platform_label(&self.sub_platform);
        // #574: never emit a dangling `<socialapi:>`. When we have no handle
        // at all, the display name alone is more honest than an empty field.
        let handle = self.author.trim();
        let from = match (&label, handle.is_empty()) {
            (Some(_), false) => {
                format!("{} <socialapi:{}:{}>", self.with, self.sub_platform, handle)
            }
            (Some(_), true) => format!("{} <socialapi:{}>", self.with, self.sub_platform),
            (None, false) => format!("{} <socialapi:{}>", self.with, handle),
            (None, true) => self.with.clone(),
        };
        let subject = match &label {
            Some(p) => format!("[{p} DM from {}]", self.with),
            None => format!("[DM from {}]", self.with),
        };
        // #573: put the shared media in the BODY, not just a card marker.
        // `emails.body` is what the triage and draft prompts read, so this is
        // the difference between the model knowing "they sent a Reel" and it
        // reporting that the message was empty. Appended rather than
        // substituted so a caption plus a Reel keeps both.
        let body = match self.attachment_url.as_deref() {
            Some(url) if self.text.trim().is_empty() => {
                format!("[shared media, no caption]\n{url}")
            }
            Some(url) => format!("{}\n\n[shared media]\n{url}", self.text),
            None => self.text,
        };
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: self.message_id,
            thread_id: Some(self.conversation_id),
            from,
            subject,
            body,
            date: self.created_at,
            account_entity_id: Some(self.account_id),
            platform: PLATFORM.to_string(),
            kind: work_item_kind::DM.to_string(),
        }
    }
}

/// The newest contiguous run of positively-incoming messages — everything the
/// counterparty sent AFTER our latest reply. `msgs` is newest-first (the
/// API's order). The scan stops at the first message that is not stated
/// `incoming` — our own outgoing OR an unstated direction — so an
/// unattributable message conservatively closes the tail rather than risking
/// a draft that replies to ourselves (#526's invariant, now provider-stated).
/// Anything behind that boundary was already answered; we draft nothing for
/// it.
fn unanswered_incoming_tail(msgs: &[DmMessage]) -> &[DmMessage] {
    let end = msgs
        .iter()
        .position(|m| !m.is_incoming())
        .unwrap_or(msgs.len());
    &msgs[..end]
}

/// True iff `created_at` (RFC3339) is within [`DM_MAX_AGE_DAYS`] of `now`.
/// Unparseable timestamps count as within, so a provider sending
/// `created_at: null` never silently loses the DM. #795 note: re-emission
/// is no longer bounded by the seen-ledger (an errored DM is deliberately
/// left unseen), so the bound on repeated work is
/// [`DM_MAX_TRIAGE_RETRIES`] in `handle_dm`, not this horizon.
fn within_horizon(created_at: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(t) => {
            now.signed_duration_since(t.with_timezone(&chrono::Utc))
                <= chrono::Duration::days(DM_MAX_AGE_DAYS)
        }
        Err(_) => true,
    }
}

/// Canonical handle form for identity comparisons: trimmed, no leading `@`,
/// lowercased. Shared by [`is_inbound`] and [`is_own_handle`] so the poll and
/// webhook paths agree on what counts as the same account.
/// Best available handle for the sender (#574).
///
/// `sender_name` comes back empty from the live API often enough that cards
/// rendered as `Muhammad Rashid <socialapi:>` — a display name followed by an
/// empty handle, which reads as a truncated field. Falls back to the stable
/// `sender_id` before giving up.
fn sender_handle(msg: &DmMessage) -> String {
    if !msg.sender_name.trim().is_empty() {
        return msg.sender_name.clone();
    }
    msg.sender_id.trim().to_string()
}

fn norm_handle(s: &str) -> String {
    s.trim().trim_start_matches('@').to_ascii_lowercase()
}

/// True iff `author` is one of OUR registered SocialAPI.ai account handles —
/// i.e. the message is our own outbound, not something to draft a reply to.
///
/// One half of the ownership signal for the #249 webhook fast path, which has
/// no conversation to compare against. `handles` comes from
/// `Store::socialapi_account_handles` and is already normalized; `author` is
/// normalized here.
///
/// Returns false against an EMPTY `handles` — for every author, including our
/// own. Callers must therefore check `handles.is_empty()` separately and treat
/// it as *unattributable* rather than *not ours*; the drain does, and defers
/// those events to the poll path. `is_inbound` is not a backstop here: it only
/// runs in the poll loop, which a webhook-delivered event never enters.
pub(crate) fn is_own_handle(handles: &[String], author: &str) -> bool {
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
    /// processed, and — for the ones not already settled in `socialapi_seen_dms`
    /// — emits a `dm` WorkItem. Reusing the same dedup ledger as the poll path
    /// means a webhook-delivered DM and a later poll of the same DM collapse to
    /// a single draft. Returns the emitted work items; the API poll runs after
    /// this with whatever budget remains. Best-effort: a malformed row is
    /// marked processed (so it doesn't wedge the queue) and skipped.
    ///
    /// Every emitted `(conversation_id, message_id)` is recorded in `emitted`
    /// because the ledger is now written by the handler, not here (#671): the
    /// in-tick set is what stops this drain and the API poll from emitting the
    /// same message twice in one tick. Conversely, a webhook row whose handler
    /// then failed is already `processed` and can only come back via the API
    /// poll — which reaches the top [`DM_CONVERSATIONS_PER_POLL`] conversations
    /// per account, and an unanswered DM is by definition in a recently-active
    /// thread.
    fn drain_webhook_events(
        &self,
        budget: u32,
        emitted: &mut HashSet<(String, String)>,
    ) -> anyhow::Result<Vec<WorkItem>> {
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
            // forever; the seen-ledger is the authoritative dedup once the
            // handler settles the DM, and the API poll is the fallback until
            // then.
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
            let stated_outbound = wp.outbound;
            let payload: SocialApiDmPayload = wp.into();

            // Attribution walk (#526). The push has no conversation to compare
            // against, so we decide direction in this order:
            //   1. the provider said so outright — authoritative;
            //   2. the author is one of our registered handles — ours;
            //   3. we have no handles at all — UNATTRIBUTABLE.
            //
            // Case 3 is the one that matters. `is_own_handle` returns false for
            // every author against an empty set, so treating "no handles" as
            // "not ours" silently reinstates the bug for any install whose
            // `account_handle` column is NULL — which is every install that
            // hasn't run Sync accounts, since the dashboard populates it
            // best-effort from a third-party payload. Instead we drop the event
            // WITHOUT writing the seen-ledger: the 5-minute poll re-surfaces
            // the same message with a real conversation to check `is_inbound`
            // against. Cost is a few minutes of latency; the alternative is
            // drafting a reply to ourselves.
            if stated_outbound || is_own_handle(&own_handles, &payload.author) {
                debug!(
                    event = %ev.id,
                    author = %payload.author,
                    stated_outbound,
                    "socialapi dm webhook: skipping our own outbound message"
                );
                continue;
            }
            if own_handles.is_empty() {
                warn!(
                    event = %ev.id,
                    "socialapi dm webhook: no registered account handles and the push \
                     did not state direction — cannot tell inbound from outbound, \
                     deferring to the API poll. Run `Sync accounts` on the dashboard \
                     to enable the webhook fast path."
                );
                continue;
            }
            // Durable dedup keyed on (conversation_id, message_id) — the SAME
            // ledger the poll path reads, so no double-draft.
            if self
                .store
                .is_socialapi_dm_seen(&payload.conversation_id, &payload.message_id)?
            {
                continue;
            }
            emitted.insert((payload.conversation_id.clone(), payload.message_id.clone()));
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
        // What this tick has already emitted. The ledger is only written once
        // the handler reaches a terminal outcome (#671), so it cannot be what
        // keeps the webhook drain and the API poll below from both emitting the
        // same message id within a single tick.
        let mut emitted: HashSet<(String, String)> = HashSet::new();
        // #249 fast-path: drain webhook-delivered DM events AHEAD of the API
        // poll so near-real-time pushes don't wait for the next tick. Shares
        // the `socialapi_seen_dms` dedup ledger with the poll below, so a
        // webhook item already settled by the handler is skipped when the poll
        // later sees it.
        let mut out = self.drain_webhook_events(budget, &mut emitted)?;
        budget = budget.saturating_sub(out.len() as u32);

        // Ownership backstop kept from #526: the messages endpoint states
        // direction explicitly now, but a sender matching one of our
        // registered handles is still skipped in case the provider ever
        // mislabels.
        let own_handles = self.store.socialapi_account_handles().unwrap_or_default();
        let accounts = self.store.active_socialapi_account_ids()?;
        // No registered accounts → poll the whole inbox once (account_id=None),
        // same fallback the own-post comment poller uses.
        let scopes: Vec<Option<String>> = if accounts.is_empty() {
            vec![None]
        } else {
            accounts.into_iter().map(Some).collect()
        };
        let now = chrono::Utc::now();

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
            for conv in conversations.iter().take(DM_CONVERSATIONS_PER_POLL) {
                if budget == 0 {
                    break;
                }
                // Wire ids are null-tolerated at decode; an id-less row is
                // unusable (no thread to fetch, no dedup key).
                if conv.id.is_empty() {
                    continue;
                }
                let msgs = match self.client.list_messages(&conv.id).await {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(conversation = %conv.id, error = %e, "dm message list failed; skipping conversation");
                        continue;
                    }
                };
                // #244 supersede: the thread's newest message being OURS means
                // the user already replied (manually, or via Approve out of
                // band) — flip any still-pending socialapi draft on the
                // conversation to `superseded` so a stale card never re-sends
                // what the user already answered. Mirrors the email outbound
                // observer; keyed on the conversation id, which is the draft's
                // `thread_id`. Best-effort: a store error here must not block
                // fetching new inbound work.
                if msgs.first().is_some_and(|m| m.is_outgoing()) {
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
                // Only the unanswered tail is draftable; walk it oldest-first
                // so multiple new messages card up chronologically.
                let tail = unanswered_incoming_tail(&msgs);
                for msg in tail.iter().rev() {
                    if budget == 0 {
                        break;
                    }
                    if msg.id.is_empty() || is_own_handle(&own_handles, &msg.sender_name) {
                        continue;
                    }
                    // Durable dedup keyed on (conversation_id, message_id),
                    // plus the in-tick set for whatever the webhook drain
                    // already emitted this tick.
                    let key = (conv.id.clone(), msg.id.clone());
                    if emitted.contains(&key) || self.store.is_socialapi_dm_seen(&key.0, &key.1)? {
                        continue;
                    }
                    if !within_horizon(&msg.created_at, now) {
                        // "Never draft this" is terminal, so the source records
                        // it — unlike emission, which leaves the ledger to the
                        // handler.
                        self.store.record_seen_socialapi_dm(
                            &conv.id,
                            &msg.id,
                            Some(msg.sender_name.as_str()),
                            Some(msg.text.as_str()),
                        )?;
                        debug!(
                            conversation = %conv.id,
                            message = %msg.id,
                            created_at = %msg.created_at,
                            "socialapi dm source: skipping stale unanswered DM (recorded as seen)"
                        );
                        continue;
                    }
                    emitted.insert(key);
                    out.push(to_work_item(conv, msg));
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
    /// Write the `socialapi_seen_dms` ledger row for a DM that has reached a
    /// terminal outcome (#671), so [`SocialApiDmSource`] stops re-feeding it.
    /// `INSERT OR IGNORE`, so re-marking an already-recorded DM is harmless.
    fn mark_seen(&self, dm: &SocialApiDmPayload) -> anyhow::Result<()> {
        self.store.record_seen_socialapi_dm(
            &dm.conversation_id,
            &dm.message_id,
            Some(dm.author.as_str()),
            Some(dm.text.as_str()),
        )?;
        Ok(())
    }

    /// Triage + draft + (unless dry-run) approval-card one inbound DM. Returns
    /// `true` when an approval card was posted.
    pub async fn handle_dm(&self, payload: SocialApiDmPayload) -> anyhow::Result<bool> {
        // The ledger is keyed on (conversation, message) and `into_email`
        // consumes the payload, so keep a copy for the terminal `mark_seen`.
        let dm = payload.clone();
        let email = payload.into_email();
        self.store.upsert_email(&email)?;
        // #795 — an `error` row is NOT settled: it means triage or the
        // approval post failed with **no card posted**, and nothing retries
        // socialapi error rows (`list_retryable_replies` is gmail-scoped
        // since #670; the reconcile sweep only touches `pending`). Marking
        // those seen dropped the DM permanently — never answered, never
        // carded, never surfaced. Gmail deliberately keeps error rows open
        // for its retry tick (channel.rs `has_open_action`); this mirrors
        // that, bounded so a permanently-failing DM cannot re-spawn a ~24k
        // token triage call on every poll forever (#448).
        match self.store.latest_action_for_message(&email.message_id)? {
            None => {}
            Some((action_id, status, retries)) if status == ActionStatus::Error.as_str() => {
                if retries >= DM_MAX_TRIAGE_RETRIES {
                    // Out of attempts. Flag rather than drop: a flagged row
                    // surfaces in the digest, a ledgered one is invisible.
                    self.store.update_action_status(
                        &action_id,
                        ActionStatus::Flagged,
                        None,
                        Some(
                            "socialapi DM triage failed repeatedly; needs attention \
                             (auto-retries exhausted)",
                        ),
                    )?;
                    self.mark_seen(&dm)?;
                    warn!(
                        message_id = %email.message_id,
                        retries,
                        "socialapi DM flagged after repeated triage failures"
                    );
                    return Ok(false);
                }
                // Leave the DM UNSEEN so the next poll re-triages it.
                // `i64::MAX` because the bound is enforced above: the store
                // helper's own threshold would flip the row to
                // `permanent_error`, which is just as silent as the drop
                // this fix removes.
                self.store.increment_retry_count(&action_id, i64::MAX)?;
                return Ok(false);
            }
            // Card is up, or the message reached a terminal outcome.
            Some(_) => {
                self.mark_seen(&dm)?;
                return Ok(false);
            }
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
            self.mark_seen(&dm)?;
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
                    // Deliberately NOT marked seen: a deferral means "try
                    // later", so the next poll must re-feed this DM.
                    info!(dm = %email.message_id, "socialapi dm reply deferred by governor: {d}");
                    return Ok(false);
                }
            }
        } else {
            None
        };

        let skill_system =
            std::fs::read_to_string(self.config.skill_dir.join("SKILL.md")).unwrap_or_default();
        let draft_opts = socialapi_draft_opts(skill_system, self.config.wiki_root.clone());
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
            self.mark_seen(&dm)?;
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
        self.mark_seen(&dm)?;
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

    async fn mount_messages(server: &MockServer, conversation_id: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/inbox/conversations/{conversation_id}/messages"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// RFC3339 timestamp `hours` ago — the DM poll has a freshness horizon
    /// ([`DM_MAX_AGE_DAYS`]), so fixtures must use relative times.
    fn rfc3339_ago(hours: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339()
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

    /// Stands in for a reasoner outage — a Claude quota refusal, a provider
    /// 5xx, anything that surfaces as `Err` rather than as a bad decision.
    struct FailingReasoner;
    #[async_trait]
    impl Reasoner for FailingReasoner {
        async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("quota refused"))
        }
    }

    /// Succeeds for the first `fail_after` calls, then fails — so the failure
    /// can be aimed at the DRAFT stage, past a successful triage.
    struct FlakyReasoner {
        fail_after: usize,
        calls: std::sync::atomic::AtomicUsize,
        inner: ScriptedReasoner,
    }
    #[async_trait]
    impl Reasoner for FlakyReasoner {
        async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= self.fail_after {
                return Err(anyhow::anyhow!("quota refused"));
            }
            self.inner.call(opts, prompt).await
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

    fn channel<R: Reasoner>(
        store: Arc<Store>,
        reasoner: Arc<R>,
        broker: Arc<RecordingBroker>,
        dry_run: bool,
    ) -> SocialApiDmChannel<R> {
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
    fn tail_is_the_incoming_run_newer_than_our_latest_reply() {
        let m = |id: &str, dir: &str| DmMessage {
            id: id.into(),
            direction: dir.into(),
            ..Default::default()
        };
        // Newest-first: two fresh incoming, then our reply, then older noise.
        let msgs = vec![
            m("m4", "incoming"),
            m("m3", "incoming"),
            m("m2", "outgoing"),
            m("m1", "incoming"),
        ];
        let tail = unanswered_incoming_tail(&msgs);
        assert_eq!(
            tail.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["m4", "m3"]
        );
        // Answered thread: newest message is ours → nothing to draft.
        assert!(unanswered_incoming_tail(&[m("m2", "outgoing"), m("m1", "incoming")]).is_empty());
        // An unstated direction conservatively closes the tail (#526).
        assert!(unanswered_incoming_tail(&[m("mx", ""), m("m1", "incoming")]).is_empty());
    }

    #[test]
    fn horizon_filters_stale_but_tolerates_garbage() {
        let now = chrono::Utc::now();
        assert!(within_horizon(
            &(now - chrono::Duration::hours(2)).to_rfc3339(),
            now
        ));
        assert!(!within_horizon(
            &(now - chrono::Duration::days(DM_MAX_AGE_DAYS + 1)).to_rfc3339(),
            now
        ));
        assert!(within_horizon("not-a-time", now));
    }

    #[tokio::test]
    async fn source_yields_unanswered_tail_once_then_dedups() {
        let (store, _f) = tmp_store();
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!({"data": [{
                "id": "conv_1",
                "account_id": "acc_1",
                "participant_name": "jane",
                "last_message": "you around?",
                "last_message_at": rfc3339_ago(1)
            }]}),
        )
        .await;
        // Newest-first, as the live API returns them: jane's fresh follow-up,
        // our reply, jane's original (already answered by m2).
        mount_messages(
            &server,
            "conv_1",
            serde_json::json!({"data": [
                {"id":"m3","direction":"incoming","sender_name":"jane","text":"you around?","created_at": rfc3339_ago(1)},
                {"id":"m2","direction":"outgoing","sender_name":"me","text":"our outbound","created_at": rfc3339_ago(2)},
                {"id":"m1","direction":"incoming","sender_name":"jane","text":"hey there","created_at": rfc3339_ago(3)}
            ]}),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);
        // Only the unanswered tail (m3) surfaces: m1 was answered by our m2,
        // and our own outbound never drafts.
        let first = source.fetch_new().await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kind, work_item_kind::DM);
        assert_eq!(first[0].platform, PLATFORM);
        assert_eq!(first[0].external_id, "m3");
        // Emitting does NOT ledger the message (#671) — that's the handler's
        // job, at a terminal outcome.
        assert!(!store.is_socialapi_dm_seen("conv_1", "m3").unwrap());
        skip_channel(Arc::clone(&store))
            .handle(first[0].clone())
            .await
            .unwrap();
        // Second poll → now in socialapi_seen_dms → empty.
        let second = source.fetch_new().await.unwrap();
        assert!(second.is_empty());
    }

    /// First-poll-after-connect guard: an unanswered DM older than
    /// [`DM_MAX_AGE_DAYS`] is written to the seen-ledger but never drafted.
    #[tokio::test]
    async fn source_skips_stale_unanswered_dms() {
        let (store, _f) = tmp_store();
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!({"data": [{
                "id": "conv_old", "account_id": "acc_1", "participant_name": "jane"
            }]}),
        )
        .await;
        mount_messages(
            &server,
            "conv_old",
            serde_json::json!({"data": [
                {"id":"m_old","direction":"incoming","sender_name":"jane","text":"hello from months ago",
                 "created_at": rfc3339_ago(24 * (DM_MAX_AGE_DAYS + 2))}
            ]}),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);
        assert!(source.fetch_new().await.unwrap().is_empty());
        // Recorded as seen: the same message can never resurface.
        assert!(
            !store
                .record_seen_socialapi_dm("conv_old", "m_old", None, None)
                .unwrap(),
            "stale DM must have been written to the seen-ledger"
        );
    }

    /// #244 supersede: a manual (outbound) reply in a conversation flips any
    /// still-pending socialapi draft on that thread to `superseded` on the
    /// next source poll, mirroring the email outbound observer.
    #[tokio::test]
    async fn source_supersedes_pending_draft_on_manual_reply() {
        let (store, _f) = tmp_store();
        // Seed a pending socialapi DM draft on conversation conv_1.
        let email = SocialApiDmPayload {
            attachment_url: None,
            sub_platform: "instagram".into(),
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

        // The thread's newest message is now our own outbound → user replied.
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!({"data": [{
                "id": "conv_1", "account_id": "acc_1", "participant_name": "jane"
            }]}),
        )
        .await;
        mount_messages(
            &server,
            "conv_1",
            serde_json::json!({"data": [
                {"id":"m2","direction":"outgoing","sender_name":"me","text":"manual reply","created_at": rfc3339_ago(1)},
                {"id":"m1","direction":"incoming","sender_name":"jane","text":"hey","created_at": rfc3339_ago(2)}
            ]}),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);
        let _ = source.fetch_new().await.unwrap();

        let row = store.get_action_with_email(&action_id).unwrap().unwrap();
        assert_eq!(row.action.status, "superseded");
    }

    /// #249 fast-path: a webhook-delivered DM event drained from
    /// `socialapi_webhook_events` surfaces as a `dm` WorkItem exactly once —
    /// the in-tick emitted set stops the API poll from re-emitting the same
    /// message id in the same tick — and once the handler carries it to a
    /// terminal outcome the shared `socialapi_seen_dms` ledger keeps every
    /// later poll quiet. In between, the poll is the fallback for a webhook
    /// item whose handler failed (#671).
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

        // The API poll returns the SAME message (m1) as fresh unanswered tail.
        let server = MockServer::start().await;
        mount_conversations(
            &server,
            serde_json::json!({"data": [{
                "id": "conv_1", "account_id": "acc_1", "participant_name": "jane"
            }]}),
        )
        .await;
        mount_messages(
            &server,
            "conv_1",
            serde_json::json!({"data": [
                {"id":"m1","direction":"incoming","sender_name":"jane","text":"hey there","created_at": rfc3339_ago(1)}
            ]}),
        )
        .await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);

        // First fetch: drains the webhook event → 1 work item (the poll sees m1
        // in the same tick, but the emitted set suppresses the double).
        let first = source.fetch_new().await.unwrap();
        assert_eq!(first.len(), 1, "webhook drain should surface m1 exactly once");
        assert_eq!(first[0].external_id, "m1");
        assert_eq!(first[0].kind, work_item_kind::DM);

        // The webhook row is consumed, so with no terminal outcome recorded the
        // API poll is the fallback — and it re-emits m1 exactly once.
        let second = source.fetch_new().await.unwrap();
        assert_eq!(
            second.len(),
            1,
            "an unhandled webhook DM must fall back to the poll"
        );
        assert_eq!(second[0].external_id, "m1");

        // Once the handler settles it, every later poll is quiet.
        skip_channel(Arc::clone(&store))
            .handle(second[0].clone())
            .await
            .unwrap();
        let third = source.fetch_new().await.unwrap();
        assert!(third.is_empty(), "no duplicate from webhook+poll convergence");
    }

    /// One conversation carrying a single fresh incoming message, for tests
    /// that poll the same inbox several times.
    async fn mount_single_incoming(server: &MockServer) {
        mount_conversations(
            server,
            serde_json::json!({"data": [{
                "id": "conv_1", "account_id": "acc_1", "participant_name": "jane"
            }]}),
        )
        .await;
        mount_messages(
            server,
            "conv_1",
            serde_json::json!({"data": [
                {"id":"m1","direction":"incoming","sender_name":"jane","text":"you around?","created_at": rfc3339_ago(1)}
            ]}),
        )
        .await;
    }

    fn skip_channel(store: Arc<Store>) -> SocialApiDmChannel<ScriptedReasoner> {
        channel(
            store,
            Arc::new(ScriptedReasoner::new([
                r#"{"decision":"skip","reason":"noise"}"#,
            ])),
            Arc::new(RecordingBroker::default()),
            false,
        )
    }

    /// #671: the seen-ledger is a TERMINAL-outcome marker, not an emission
    /// receipt. A reasoner outage during triage must leave the DM unledgered so
    /// the next poll re-feeds it — otherwise a transient quota refusal silently
    /// drops a human's message forever.
    #[tokio::test]
    async fn reasoner_error_leaves_dm_unseen_so_next_poll_refeeds_it() {
        let (store, _f) = tmp_store();
        let server = MockServer::start().await;
        mount_single_incoming(&server).await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);

        let first = source.fetch_new().await.unwrap();
        assert_eq!(first.len(), 1);

        let failing = channel(
            Arc::clone(&store),
            Arc::new(FailingReasoner),
            Arc::new(RecordingBroker::default()),
            false,
        );
        assert!(failing.handle(first[0].clone()).await.is_err());
        assert!(!store.is_socialapi_dm_seen("conv_1", "m1").unwrap());
        assert!(!store.is_message_processed("m1").unwrap());

        // The next poll re-feeds the same message...
        let second = source.fetch_new().await.unwrap();
        assert_eq!(second.len(), 1, "a failed DM must be re-fed");
        assert_eq!(second[0].external_id, "m1");

        // ...and once the reasoner recovers, the terminal outcome ledgers it.
        let broker = Arc::new(RecordingBroker::default());
        let ch = channel(
            Arc::clone(&store),
            Arc::new(ScriptedReasoner::new([
                r#"{"decision":"reply","reason":"genuine question"}"#,
                "Sure, how about Thursday?",
            ])),
            Arc::clone(&broker),
            false,
        );
        ch.handle(second[0].clone()).await.unwrap();
        assert_eq!(broker.posts.lock().unwrap().len(), 1);
        assert!(store.is_socialapi_dm_seen("conv_1", "m1").unwrap());
        assert!(source.fetch_new().await.unwrap().is_empty());
    }

    /// Same guarantee one stage later: triage succeeded, the draft call blew
    /// up. No action row, no ledger row, re-fed on the next poll.
    #[tokio::test]
    async fn draft_stage_error_also_refeeds() {
        let (store, _f) = tmp_store();
        let server = MockServer::start().await;
        mount_single_incoming(&server).await;
        let source = SocialApiDmSource::new(client(&server), Arc::clone(&store), 10);
        let first = source.fetch_new().await.unwrap();
        assert_eq!(first.len(), 1);

        let ch = channel(
            Arc::clone(&store),
            Arc::new(FlakyReasoner {
                fail_after: 1,
                calls: std::sync::atomic::AtomicUsize::new(0),
                inner: ScriptedReasoner::new([r#"{"decision":"reply","reason":"genuine"}"#]),
            }),
            Arc::new(RecordingBroker::default()),
            false,
        );
        assert!(ch.handle(first[0].clone()).await.is_err());
        assert!(!store.is_socialapi_dm_seen("conv_1", "m1").unwrap());
        assert!(!store.is_message_processed("m1").unwrap());

        let second = source.fetch_new().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].external_id, "m1");
    }

    /// A message that already carries an action row — an Error row from an
    /// earlier parse failure, say — is settled as far as the handler is
    /// concerned, so it must be ledgered. Without this the source re-feeds it
    /// on every poll, forever.
    /// #795 — an `error` row means triage/approval failed with NO card
    /// posted, and nothing retries socialapi error rows. Sealing it with
    /// `mark_seen` dropped the DM forever; it must stay re-feedable until
    /// the retry bound, then surface as flagged rather than vanish.
    #[tokio::test]
    async fn errored_dm_stays_refeedable_then_flags_instead_of_dropping() {
        let (store, _f) = tmp_store();
        let action = store
            .log_action(
                "m_e",
                Some("conv_e"),
                "jane <socialapi:jane>",
                "[DM from jane]",
                Some("you around?"),
                None,
                ActionStatus::Error,
            )
            .unwrap();
        let ch = skip_channel(Arc::clone(&store));

        // Under the bound: no card, no ledger write, retry counted.
        for expected in 1..=DM_MAX_TRIAGE_RETRIES {
            assert!(!ch.handle_dm(SocialApiDmPayload {
                attachment_url: None,
                sub_platform: "instagram".into(),
                conversation_id: "conv_e".into(),
                account_id: "acc_1".into(),
                with: "jane".into(),
                message_id: "m_e".into(),
                author: "jane".into(),
                text: "you around?".into(),
                created_at: rfc3339_ago(1),
            }).await.unwrap());
            assert!(
                !store.is_socialapi_dm_seen("conv_e", "m_e").unwrap(),
                "errored DM must stay unseen so the next poll retries it"
            );
            let (_, status, retries) =
                store.latest_action_for_message("m_e").unwrap().unwrap();
            assert_eq!(status, ActionStatus::Error.as_str());
            assert_eq!(retries, expected, "retry must be counted");
        }

        // Bound reached: flagged (visible in the digest) and ledgered.
        assert!(!ch.handle_dm(SocialApiDmPayload {
                attachment_url: None,
                sub_platform: "instagram".into(),
                conversation_id: "conv_e".into(),
                account_id: "acc_1".into(),
                with: "jane".into(),
                message_id: "m_e".into(),
                author: "jane".into(),
                text: "you around?".into(),
                created_at: rfc3339_ago(1),
            }).await.unwrap());
        let (id, status, _) = store.latest_action_for_message("m_e").unwrap().unwrap();
        assert_eq!(id, action);
        assert_eq!(
            status,
            ActionStatus::Flagged.as_str(),
            "exhausted retries must FLAG, never silently drop"
        );
        assert!(store.is_socialapi_dm_seen("conv_e", "m_e").unwrap());
    }

    #[tokio::test]
    async fn already_processed_message_is_marked_seen() {
        let (store, _f) = tmp_store();
        // #795: a TERMINAL row (skipped) is genuinely settled. An `error`
        // row is not — see errored_dm_stays_refeedable_then_flags_instead_of_dropping.
        store
            .log_action(
                "m1",
                Some("conv_1"),
                "jane <socialapi:jane>",
                "[DM from jane]",
                Some("you around?"),
                None,
                ActionStatus::Skipped,
            )
            .unwrap();
        let ch = skip_channel(Arc::clone(&store));
        let posted = ch
            .handle_dm(SocialApiDmPayload {
                attachment_url: None,
                sub_platform: "instagram".into(),
                conversation_id: "conv_1".into(),
                account_id: "acc_1".into(),
                with: "jane".into(),
                message_id: "m1".into(),
                author: "jane".into(),
                text: "you around?".into(),
                created_at: rfc3339_ago(1),
            })
            .await
            .unwrap();
        assert!(!posted);
        assert!(store.is_socialapi_dm_seen("conv_1", "m1").unwrap());
    }

    /// A dry-run draft is a terminal outcome too — no card, but the DM is
    /// handled and must not be re-fed.
    #[tokio::test]
    async fn dry_run_decision_marks_seen_without_a_card() {
        let (store, _f) = tmp_store();
        let reasoner = Arc::new(ScriptedReasoner::new([
            r#"{"decision":"reply","reason":"genuine question"}"#,
            "Sure, how about Thursday?",
        ]));
        let broker = Arc::new(RecordingBroker::default());
        let ch = channel(Arc::clone(&store), reasoner, Arc::clone(&broker), true);
        assert!(!ch.handle_dm(dm_payload("instagram")).await.unwrap());
        assert!(broker.posts.lock().unwrap().is_empty());
        assert!(store.is_socialapi_dm_seen("c1", "m1").unwrap());
    }

    #[tokio::test]
    async fn reply_decision_posts_approval_card() {
        let (store, _f) = tmp_store();
        let payload = SocialApiDmPayload {
            attachment_url: None,
            sub_platform: "instagram".into(),
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
        assert!(store.is_socialapi_dm_seen("conv_1", "m1").unwrap());
    }

    #[tokio::test]
    async fn skip_decision_posts_no_card() {
        let (store, _f) = tmp_store();
        let payload = SocialApiDmPayload {
            attachment_url: None,
            sub_platform: "instagram".into(),
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
        assert!(store.is_socialapi_dm_seen("conv_1", "spam1").unwrap());
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
        // The live API's empty shape is `{"data":null}`, not `[]` (#543).
        mount_conversations(&server, serde_json::json!({"data": null})).await;

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
        // The live API's empty shape is `{"data":null}`, not `[]` (#543).
        mount_conversations(&server, serde_json::json!({"data": null})).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        let items = src.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "msg_2");
        assert_eq!(items[0].kind, work_item_kind::DM);
    }

    /// With no registered handles and no stated direction we CANNOT tell
    /// inbound from outbound. Emitting would reinstate #526 on every install
    /// whose account_handle column is still NULL, so the drain defers to the
    /// poll — and must NOT write the seen-ledger, or the poll could never
    /// resurface the message.
    #[tokio::test]
    async fn drain_defers_unattributable_event_to_the_poll() {
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
        // The live API's empty shape is `{"data":null}`, not `[]` (#543).
        mount_conversations(&server, serde_json::json!({"data": null})).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        assert!(src.fetch_new().await.unwrap().is_empty());
        // Processed, so it can't wedge the queue...
        assert!(store
            .take_unprocessed_socialapi_webhook_events("dm", 10)
            .unwrap()
            .is_empty());
        // ...but NOT in the seen-ledger, so the poll can still surface it with
        // a real conversation to attribute against. record_seen returns true
        // only for a genuinely new (conversation, message) pair.
        assert!(
            store
                .record_seen_socialapi_dm("conv_9", "msg_9", None, None)
                .unwrap(),
            "unattributable event must not have been recorded as seen"
        );
    }

    /// A provider that states direction outright is authoritative — no
    /// registered handles needed.
    #[tokio::test]
    async fn drain_honors_stated_outbound_direction() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        seed_dm_event(
            &store,
            "socialapi:dm:conv_3:msg_3",
            serde_json::json!({
                "type": "dm", "id": "msg_3", "conversation_id": "conv_3",
                "account_id": "acc_3", "with": "jane", "author": "acme",
                "text": "sent by us", "created_at": "2026-05-28T00:00:00Z",
                "outbound": true
            }),
        );
        // The live API's empty shape is `{"data":null}`, not `[]` (#543).
        mount_conversations(&server, serde_json::json!({"data": null})).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        assert!(src.fetch_new().await.unwrap().is_empty());
    }

    /// ...and a stated INBOUND direction still flows when handles are known.
    #[tokio::test]
    async fn drain_emits_when_direction_stated_inbound() {
        let server = MockServer::start().await;
        let (store, _f) = tmp_store();
        seed_account(&store, "acc_1", "acme");
        seed_dm_event(
            &store,
            "socialapi:dm:conv_4:msg_4",
            serde_json::json!({
                "type": "dm", "id": "msg_4", "conversation_id": "conv_4",
                "account_id": "acc_1", "with": "jane", "author": "jane",
                "text": "hi there", "created_at": "2026-05-28T00:00:00Z",
                "outbound": false
            }),
        );
        // The live API's empty shape is `{"data":null}`, not `[]` (#543).
        mount_conversations(&server, serde_json::json!({"data": null})).await;

        let src = SocialApiDmSource::new(client(&server), Arc::clone(&store), 25);
        let items = src.fetch_new().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "msg_4");
    }

    /// The receiver leaves `with` empty when the push doesn't name a
    /// counterparty; the Rust display fallback must then use `author`, which
    /// for an emitted (inbound) message IS the counterparty. Regression guard
    /// against sourcing `with` from `recipient`/`to`, which is us.
    #[test]
    fn empty_with_displays_the_author_not_our_own_account() {
        let payload: SocialApiDmPayload = serde_json::from_value::<DmWebhookPayload>(
            serde_json::json!({
                "id": "m1", "conversation_id": "c1", "account_id": "acc_1",
                "with": "", "author": "jane", "text": "hi",
                "created_at": "2026-05-28T00:00:00Z"
            }),
        )
        .unwrap()
        .into();
        assert_eq!(payload.with, "jane");
        let email = payload.into_email();
        assert_eq!(email.subject, "[DM from jane]");
        assert_eq!(email.from, "jane <socialapi:jane>");
    }

    // --- card titles must name the network the DM came from ---

    fn dm_payload(sub_platform: &str) -> SocialApiDmPayload {
        SocialApiDmPayload {
            attachment_url: None,
            sub_platform: sub_platform.into(),
            conversation_id: "c1".into(),
            account_id: "acc_1".into(),
            with: "jane".into(),
            message_id: "m1".into(),
            author: "jane".into(),
            text: "hey".into(),
            created_at: "2026-08-04T00:00:00Z".into(),
        }
    }

    /// One SocialAPI.ai key fronts several networks, so "[DM from jane]" left
    /// no way to tell which inbox a card came from.
    #[test]
    fn dm_subject_names_the_platform() {
        assert_eq!(
            dm_payload("instagram").into_email().subject,
            "[Instagram DM from jane]"
        );
        assert_eq!(dm_payload("x").into_email().subject, "[X DM from jane]");
        assert_eq!(
            dm_payload("linkedin").into_email().subject,
            "[LinkedIn DM from jane]"
        );
    }

    /// Casing is normalized, and an unknown network is passed through rather
    /// than dropped — a new platform should read oddly, not vanish.
    #[test]
    fn dm_subject_normalizes_known_and_passes_through_unknown() {
        assert_eq!(
            dm_payload("INSTAGRAM").into_email().subject,
            "[Instagram DM from jane]"
        );
        assert_eq!(
            dm_payload("twitter").into_email().subject,
            "[X DM from jane]"
        );
        assert_eq!(
            dm_payload("mastodon").into_email().subject,
            "[mastodon DM from jane]"
        );
    }

    /// An unstated platform degrades to the old title rather than printing an
    /// empty bracket like "[ DM from jane]".
    #[test]
    fn dm_subject_without_platform_falls_back_cleanly() {
        let e = dm_payload("").into_email();
        assert_eq!(e.subject, "[DM from jane]");
        assert_eq!(e.from, "jane <socialapi:jane>");
        assert_eq!(dm_payload("   ").into_email().subject, "[DM from jane]");
    }

    /// The from-line carries the network too, so the triage/draft prompts see
    /// it — register and length conventions differ per platform.
    #[test]
    fn dm_from_line_carries_the_platform() {
        assert_eq!(
            dm_payload("instagram").into_email().from,
            "jane <socialapi:instagram:jane>"
        );
    }

    #[test]
    fn platform_label_maps_aliases() {
        assert_eq!(platform_label("ig").as_deref(), Some("Instagram"));
        assert_eq!(platform_label("li").as_deref(), Some("LinkedIn"));
        assert_eq!(platform_label("tiktok").as_deref(), Some("TikTok"));
        assert_eq!(platform_label(""), None);
        assert_eq!(platform_label("  "), None);
    }

    // --- #573 shared media / #574 empty sender handle ---

    /// The exact case the operator hit: a Reel with no caption. The body must
    /// carry the URL, because `emails.body` is what the triage and draft
    /// prompts read — otherwise the model is asked to reply to nothing and
    /// correctly says so.
    #[test]
    fn shared_media_with_no_caption_reaches_the_body() {
        let mut p = dm_payload("instagram");
        p.text = String::new();
        p.attachment_url = Some("https://cdn.example/reel/1.mp4".into());
        let body = p.into_email().body;
        assert!(body.contains("https://cdn.example/reel/1.mp4"), "{body}");
        assert!(body.contains("shared media"), "{body}");
        assert!(!body.trim().is_empty());
    }

    /// A caption AND a Reel keeps both — appended, not substituted.
    #[test]
    fn caption_plus_media_keeps_both() {
        let mut p = dm_payload("instagram");
        p.text = "check this out".into();
        p.attachment_url = Some("https://cdn.example/reel/2.mp4".into());
        let body = p.into_email().body;
        assert!(body.contains("check this out"), "{body}");
        assert!(body.contains("https://cdn.example/reel/2.mp4"), "{body}");
    }

    /// A plain text DM is untouched — no marker noise on the common case.
    #[test]
    fn text_only_dm_body_is_unchanged() {
        let mut p = dm_payload("instagram");
        p.text = "just text".into();
        p.attachment_url = None;
        assert_eq!(p.into_email().body, "just text");
    }

    /// #574: `Muhammad Rashid <socialapi:>` — a display name followed by an
    /// empty handle. With no handle at all, the name alone is more honest.
    #[test]
    fn empty_sender_handle_never_renders_a_dangling_bracket() {
        let mut p = dm_payload("");
        p.author = String::new();
        let from = p.into_email().from;
        assert_eq!(from, "jane");
        assert!(!from.contains("socialapi:"), "{from}");

        let mut p2 = dm_payload("instagram");
        p2.author = "   ".into();
        let from2 = p2.into_email().from;
        assert_eq!(from2, "jane <socialapi:instagram>");
        assert!(!from2.contains("instagram:>"), "{from2}");
    }

    /// `sender_handle` prefers the name, falls back to the stable id.
    #[test]
    fn sender_handle_falls_back_to_sender_id() {
        let mut msg = DmMessage::default();
        msg.sender_name = "  ".into();
        msg.sender_id = "ig_12345".into();
        assert_eq!(sender_handle(&msg), "ig_12345");
        msg.sender_name = "jane".into();
        assert_eq!(sender_handle(&msg), "jane");
    }
}
