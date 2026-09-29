//! #1289 — the approval workflow on Slack.
//!
//! [`SlackApprovals`] is one Slack approval surface: it posts cards
//! ([`ApprovalBroker`]), redraws them in place when anything decides their
//! action ([`ApprovalCardSurface`]), and turns the owner's clicks, modal
//! submissions and text commands into the *same* [`ApprovalActionHandler`]
//! calls the Discord bot makes — so every decision (send, skip, revise,
//! quick-refine preset, missing info, recompose) runs the same business
//! logic, store compare-and-swap and recovery copy as on Discord.
//!
//! ```text
//! post_approval ──► chat.postMessage ──► surface_approval_cards (durable pointer)
//! click / modal / `approve <ref>` ──► (interactive surface, owner-gated)
//!        ──► ApprovalActionHandler (CAS: one winner across surfaces)
//!        ──► chat.update every Slack card for the action (in place)
//!        ──► CardSurfaces::redraw_except("slack") (Discord, …)
//!        ──► ephemeral reply (or a fresh message when the click was late)
//! ```
//!
//! * **Exactly once.** Decisions go through the handler, whose store
//!   transitions have one winner; a second click anywhere gets the
//!   already-resolved reply with its reason (`outcome::describe`).
//! * **Right draft.** Every control carries the digest of the draft its card
//!   showed. A click on a card whose draft has since changed is refused and
//!   the card is redrawn, so a decision never lands on a draft the owner
//!   did not see.
//! * **Durable.** Card pointers live in the store, so a restart between post
//!   and click still resolves and redraws; [`SlackApprovals::reconcile`]
//!   redraws live cards whose action moved on while nothing was watching
//!   (a sweep supersede, a dashboard decision, a crash).
//! * **Late clicks.** Slack's `trigger_id` lives three seconds. A click
//!   handled later than that (the Mac slept between ack and dispatch) cannot
//!   open a modal, so it is answered with a fresh message carrying the text
//!   command instead; decisions still run, and their answer is a fresh
//!   message rather than an ephemeral one the owner might never see.
//!
//! Scheduling controls (#1291) are not drawn on Slack yet; scheduled notices
//! stay on Discord (`post_scheduled_notice` keeps the no-notice default).

pub mod card;

use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use augmentagent_approval_discord::outcome::{
    card_status_line, describe, offers_recompose, redraft_produced_no_card,
};
use augmentagent_approval_discord::{
    append_envelope_markers, fill_feedback, revise_result_prefix, split_needs_input,
    ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalCardSurface,
    ApprovalError, CardSurfaces, MAX_REDRAFT_ITERATIONS, PRESETS,
};
use augmentagent_store::approval_cards::{ActionRefMatch, ApprovalCardPointer, ApprovalCardState};
use augmentagent_store::{ActionWithEmail, Email, Store, SurfaceMessageRef, SurfacePlatform};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::surface::{SlackWorkspace, SLACK_SURFACE_PLATFORM};
use crate::transport::event::{Interaction, InteractionKind};
use crate::transport::web::{PostEphemeral, PostMessage, SlackWebApi, UpdateMessage};

use card::{CardInput, ControlRef, ModalContext};

/// Surface name used for cross-surface sync and logs.
pub const SURFACE: &str = "slack";

/// Slack's `trigger_id` expires three seconds after the click
/// (docs.slack.dev, "Handling user interaction", read 2026-09-29).
pub const TRIGGER_TTL_MS: i64 = 3_000;

pub const NO_HANDLER_REPLY: &str =
    "Approvals are not available on this daemon right now: no approval handler is configured.";
pub const STALE_DRAFT_REPLY: &str =
    "This card showed an earlier draft. It now shows the current one — review it and decide again.";
pub const REFINE_LIMIT_REPLY: &str =
    "Refine limit reached for this draft — Approve, Skip, or use Revise for a free-form edit.";
pub const REVISED_REPLY: &str = "Revised — the card now shows the new draft.";
pub const NO_VALUES_REPLY: &str = "No values supplied — draft unchanged.";

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `1700000000.123456` → milliseconds.
fn slack_ts_ms(ts: &str) -> Option<i64> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    let secs: i64 = secs.parse().ok()?;
    let frac: String = frac.chars().take(3).collect();
    let millis: i64 = format!("{frac:0<3}").parse().ok()?;
    Some(secs * 1000 + millis)
}

/// Where cards go and who they are for.
#[derive(Debug, Clone)]
pub struct SlackApprovalConfig {
    pub workspace: SlackWorkspace,
    /// The owner DM or bound control channel new cards are posted to.
    pub channel: String,
}

/// One Slack approval surface. Build with [`new`](Self::new), give it the
/// handler with [`set_handler`](Self::set_handler) once the daemon built
/// one, and [`register`](Self::register) it for cross-surface redraws.
pub struct SlackApprovals {
    store: Arc<Store>,
    web: Arc<dyn SlackWebApi>,
    config: SlackApprovalConfig,
    handler: OnceLock<Arc<dyn ApprovalActionHandler>>,
    surfaces: CardSurfaces,
    clock: Clock,
}

/// The decision a control or command asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verb {
    Approve,
    Skip,
    Revise(String),
    Refine(String),
    Recompose,
}

/// Where the answer to a decision goes.
struct ReplyTo {
    channel: String,
    user: Option<String>,
    /// Answer with a fresh message instead of an ephemeral one.
    fresh: bool,
}

impl SlackApprovals {
    pub fn new(
        store: Arc<Store>,
        web: Arc<dyn SlackWebApi>,
        config: SlackApprovalConfig,
        surfaces: CardSurfaces,
    ) -> Self {
        Self {
            store,
            web,
            config,
            handler: OnceLock::new(),
            surfaces,
            clock: Arc::new(system_now_ms),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The business logic every decision goes through. Set once.
    pub fn set_handler(&self, handler: Arc<dyn ApprovalActionHandler>) {
        if self.handler.set(handler).is_err() {
            warn!("slack approvals: handler already set; keeping the first");
        }
    }

    /// Join the daemon's card surfaces so decisions elsewhere redraw ours.
    pub fn register(self: &Arc<Self>) {
        let me: Arc<dyn ApprovalCardSurface> = self.clone();
        self.surfaces.register(&me);
    }

    pub fn workspace(&self) -> &SlackWorkspace {
        &self.config.workspace
    }

    pub fn channel(&self) -> &str {
        &self.config.channel
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn platform() -> SurfacePlatform {
        SurfacePlatform::new(SLACK_SURFACE_PLATFORM).expect("static platform")
    }

    fn message_ref(&self, channel: &str, ts: &str) -> Option<SurfaceMessageRef> {
        let conversation = self.config.workspace.conversation(channel, None).ok()?;
        SurfaceMessageRef::new(conversation, ts).ok()
    }

    fn load(&self, action_id: &str) -> Option<ActionWithEmail> {
        match self.store.get_action_with_email(action_id) {
            Ok(row) => row,
            Err(e) => {
                warn!(action_id, "slack approvals: could not load the action: {e}");
                None
            }
        }
    }

    fn redraft_count(&self, action_id: &str) -> i64 {
        self.store.redraft_count(action_id).unwrap_or(0)
    }

    // -----------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------

    /// `(text, blocks, digest, status)` for the action's current state.
    fn render(&self, row: &ActionWithEmail, note: Option<&str>) -> (String, Value, String, String) {
        let action_id = row.action.id.as_str();
        let stored = row.action.draft_body.clone().unwrap_or_default();
        let digest = card::draft_digest(&stored);
        let display = append_envelope_markers(
            stored,
            Some(self.store.as_ref()),
            action_id,
            &row.email.from,
            None,
        );
        let status = row.action.status.clone();
        let detail = row.action.error_message.as_deref();
        let line = card_status_line(&status, detail);
        let (text, blocks) = card::card(&CardInput {
            action_id,
            email: &row.email,
            display_draft: &display,
            digest: &digest,
            redraft_count: self.redraft_count(action_id),
            note: if line.is_none() { note } else { None },
            status_line: line.as_deref(),
            offer_recompose: offers_recompose(&status, detail),
        });
        (text, blocks, digest, status)
    }

    /// Post a new card for `action_id` and record where it went. Older live
    /// cards for the same action are marked replaced and redrawn to point
    /// at the new one, so exactly one Slack card is actionable.
    async fn post_card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        note: Option<&str>,
    ) -> Result<(), ApprovalError> {
        let (text, blocks, digest, status) = match self.load(action_id) {
            Some(row) => self.render(&row, note),
            // A caller that has not persisted the action (tests, tools):
            // draw what it passed.
            None => {
                let digest = card::draft_digest(draft);
                let (text, blocks) = card::card(&CardInput {
                    action_id,
                    email,
                    display_draft: draft,
                    digest: &digest,
                    redraft_count: 0,
                    note,
                    status_line: None,
                    offer_recompose: false,
                });
                (text, blocks, digest, "pending".to_string())
            }
        };
        let previous = self
            .store
            .approval_cards_for_action(&Self::platform(), action_id)
            .unwrap_or_default();
        let posted = self
            .web
            .post_message(PostMessage {
                channel: self.config.channel.clone(),
                text,
                blocks: Some(blocks),
                unfurl_links: Some(false),
                link_names: Some(false),
                ..PostMessage::default()
            })
            .await
            .map_err(|e| ApprovalError::Discord(format!("slack chat.postMessage: {e}")))?;
        let now = self.now();
        match self.message_ref(&posted.channel, &posted.ts) {
            Some(message) => {
                if let Err(e) = self
                    .store
                    .record_approval_card(&message, action_id, &digest, &status, now)
                {
                    warn!(action_id, "slack approvals: could not record the card: {e}");
                }
            }
            None => warn!(action_id, "slack approvals: posted card has no usable id"),
        }
        info!(action_id, channel = %posted.channel, ts = %posted.ts, "slack approval card posted");
        for old in previous
            .into_iter()
            .filter(|c| c.state != ApprovalCardState::Replaced)
        {
            let (text, blocks) = card::replaced_card(email, action_id);
            self.update(&old, text, blocks).await;
            let _ = self.store.mark_approval_card(
                &old.message,
                ApprovalCardState::Replaced,
                &old.draft_digest,
                &old.rendered_status,
                now,
            );
        }
        Ok(())
    }

    async fn update(&self, card: &ApprovalCardPointer, text: String, blocks: Value) -> bool {
        match self
            .web
            .update_message(UpdateMessage {
                channel: card.message.conversation().conversation_id().to_string(),
                ts: card.message.message_id().to_string(),
                text,
                blocks: Some(blocks),
            })
            .await
        {
            Ok(_) => true,
            Err(e) => {
                warn!(action_id = %card.action_id, "slack approvals: chat.update failed: {e}");
                false
            }
        }
    }

    /// Redraw every Slack card for `action_id` in place, except ones a newer
    /// card replaced. A settled card is redrawn too: `sending` becomes
    /// `sent` (or `error`), and a recomposed draft is live again.
    async fn redraw_own(&self, action_id: &str, note: Option<&str>) {
        let cards = match self
            .store
            .approval_cards_for_action(&Self::platform(), action_id)
        {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    action_id,
                    "slack approvals: could not read card pointers: {e}"
                );
                return;
            }
        };
        let live: Vec<_> = cards
            .into_iter()
            .filter(|c| c.state != ApprovalCardState::Replaced)
            .collect();
        if live.is_empty() {
            return;
        }
        let now = self.now();
        match self.load(action_id) {
            Some(row) => {
                let (text, blocks, digest, status) = self.render(&row, note);
                // `sending` is transient: keep the card in the sweep until
                // the send settles.
                let state = if status == "pending" || status == "sending" {
                    ApprovalCardState::Live
                } else {
                    ApprovalCardState::Settled
                };
                for c in &live {
                    if self.update(c, text.clone(), blocks.clone()).await {
                        let _ = self
                            .store
                            .mark_approval_card(&c.message, state, &digest, &status, now);
                    }
                }
            }
            None => {
                let gone = describe(&ApprovalActionOutcome::NotFound);
                for c in &live {
                    let blocks = serde_json::json!([{
                        "type": "section",
                        "text": {"type": "mrkdwn", "text": format!("🔒 {gone}")},
                    }]);
                    if self.update(c, gone.clone(), blocks).await {
                        let _ = self.store.mark_approval_card(
                            &c.message,
                            ApprovalCardState::Settled,
                            &c.draft_digest,
                            "gone",
                            now,
                        );
                    }
                }
            }
        }
    }

    /// This surface's cards, then every other surface's.
    async fn sync(&self, action_id: &str, note: Option<&str>) {
        self.redraw_own(action_id, note).await;
        self.surfaces.redraw_except(SURFACE, action_id).await;
    }

    /// Redraw live cards whose action changed while nothing was watching
    /// (a sweep supersede, the dashboard, a daemon that died mid-decision).
    /// Returns how many actions were redrawn. Run at start and periodically.
    pub async fn reconcile(&self) -> usize {
        let live = match self.store.live_approval_cards(&Self::platform()) {
            Ok(l) => l,
            Err(e) => {
                warn!("slack approvals: reconcile could not read cards: {e}");
                return 0;
            }
        };
        let mut stale: Vec<String> = Vec::new();
        for l in live {
            let moved = match &l.action_status {
                None => true,
                Some(status) => {
                    *status != l.card.rendered_status
                        || card::draft_digest(l.draft_body.as_deref().unwrap_or_default())
                            != l.card.draft_digest
                }
            };
            if moved && !stale.contains(&l.card.action_id) {
                stale.push(l.card.action_id.clone());
            }
        }
        for action_id in &stale {
            self.redraw_own(action_id, None).await;
        }
        if !stale.is_empty() {
            info!(
                redrawn = stale.len(),
                "slack approvals: reconciled stale cards"
            );
        }
        stale.len()
    }

    // -----------------------------------------------------------------
    // Answers
    // -----------------------------------------------------------------

    /// `text` is plain text (plus `*bold*`/`_italic_`/backticks): `&`, `<`
    /// and `>` are escaped here, so a placeholder like `<what to change>`
    /// is never read as a Slack link.
    async fn reply(&self, to: &ReplyTo, text: &str, recompose_for: Option<&str>) {
        let blocks = card::reply_blocks(text, recompose_for);
        let text = crate::delivery::mrkdwn::escape(text);
        let text = text.as_str();
        let result = match (&to.user, to.fresh) {
            (Some(user), false) => self
                .web
                .post_ephemeral(PostEphemeral {
                    channel: to.channel.clone(),
                    user: user.clone(),
                    text: text.to_string(),
                    blocks,
                    thread_ts: None,
                })
                .await
                .map(|_| ()),
            _ => self
                .web
                .post_message(PostMessage {
                    channel: to.channel.clone(),
                    text: text.to_string(),
                    blocks,
                    unfurl_links: Some(false),
                    link_names: Some(false),
                    ..PostMessage::default()
                })
                .await
                .map(|_| ()),
        };
        if let Err(e) = result {
            warn!("slack approvals: could not answer the owner: {e}");
        }
    }

    fn outcome_reply(outcome: &ApprovalActionOutcome) -> (String, bool) {
        let text = match outcome {
            ApprovalActionOutcome::Revised { .. } => REVISED_REPLY.to_string(),
            other => describe(other),
        };
        let recompose = matches!(
            outcome,
            ApprovalActionOutcome::AlreadyResolved { status, detail }
                if offers_recompose(status, detail.as_deref())
        );
        (text, recompose)
    }

    // -----------------------------------------------------------------
    // Decisions
    // -----------------------------------------------------------------

    fn handler(&self) -> Option<Arc<dyn ApprovalActionHandler>> {
        self.handler.get().cloned()
    }

    /// Run one decision through the shared handler, redraw every card and
    /// return the outcome.
    async fn decide(&self, action_id: &str, verb: &Verb) -> ApprovalActionOutcome {
        let Some(handler) = self.handler() else {
            return ApprovalActionOutcome::Failed {
                message: NO_HANDLER_REPLY.into(),
            };
        };
        match verb {
            Verb::Approve => {
                let out = handler.approve(action_id).await;
                self.sync(action_id, None).await;
                out
            }
            Verb::Skip => {
                let out = handler.skip(action_id).await;
                self.sync(action_id, None).await;
                out
            }
            Verb::Recompose => {
                // The handler posts the fresh card itself (through the
                // daemon's broker, which includes this surface).
                let out = handler.recompose(action_id).await;
                self.surfaces.redraw_except(SURFACE, action_id).await;
                out
            }
            Verb::Revise(feedback) => self.redraft(&handler, action_id, feedback, None).await,
            Verb::Refine(preset_id) => {
                let Some(preset) = PRESETS.iter().find(|p| p.id == preset_id) else {
                    return ApprovalActionOutcome::Failed {
                        message: "unknown refine preset".into(),
                    };
                };
                if self.redraft_count(action_id) >= MAX_REDRAFT_ITERATIONS {
                    return ApprovalActionOutcome::Failed {
                        message: REFINE_LIMIT_REPLY.into(),
                    };
                }
                self.redraft(&handler, action_id, preset.feedback, Some(preset.id))
                    .await
            }
        }
    }

    /// Revise / quick-refine / missing info: the same steps the Discord
    /// modal and select run (#37 triple, #34 counter, #1190 notice), except
    /// that the card is updated in place instead of reposted.
    async fn redraft(
        &self,
        handler: &Arc<dyn ApprovalActionHandler>,
        action_id: &str,
        feedback: &str,
        preset: Option<&str>,
    ) -> ApprovalActionOutcome {
        let snapshot = self.load(action_id);
        let original = snapshot
            .as_ref()
            .map(|a| a.action.draft_body.clone().unwrap_or_default());
        let outcome = handler.revise(action_id, feedback).await;
        if let ApprovalActionOutcome::Revised { draft, .. } = &outcome {
            if let Some(orig) = original.as_ref() {
                if let Err(e) = self
                    .store
                    .record_revision_triple(action_id, orig, feedback, draft)
                {
                    warn!(action_id, "slack revise: could not record the triple: {e}");
                }
            }
            if let Err(e) = self.store.record_redraft(action_id, preset) {
                warn!(action_id, "slack revise: record_redraft failed: {e}");
            }
            let note = revise_result_prefix().replace("**", "*");
            self.sync(action_id, Some(&note)).await;
        } else {
            if redraft_produced_no_card(&outcome) {
                self.post_no_card_notice(snapshot.as_ref().map(|a| &a.email), &outcome)
                    .await;
            }
            self.sync(action_id, None).await;
        }
        outcome
    }

    /// #1190 on Slack: a redraft that changed nothing leaves a durable
    /// message, not only an ephemeral one.
    async fn post_no_card_notice(&self, email: Option<&Email>, outcome: &ApprovalActionOutcome) {
        let subject = email
            .map(|e| format!(" for *{}*", crate::delivery::mrkdwn::escape(&e.subject)))
            .unwrap_or_default();
        let text = format!(
            "⚠️ *Revise produced no new draft* — the approval card{subject} is unchanged.\n\
             _reason: {}_\nRetry *Revise* or *Skip* the draft.",
            crate::delivery::mrkdwn::escape(&describe(outcome))
        );
        if let Err(e) = self
            .web
            .post_message(PostMessage {
                channel: self.config.channel.clone(),
                text,
                unfurl_links: Some(false),
                link_names: Some(false),
                ..PostMessage::default()
            })
            .await
        {
            warn!("slack revise: could not post the no-draft notice: {e}");
        }
    }

    // -----------------------------------------------------------------
    // Interactions
    // -----------------------------------------------------------------

    /// Whether this surface owns `interaction` (a card control or modal).
    pub fn handles(&self, interaction: &Interaction) -> bool {
        if interaction
            .team_id
            .as_deref()
            .is_some_and(|t| t != self.config.workspace.team_id())
        {
            return false;
        }
        match interaction.kind {
            InteractionKind::BlockActions => interaction.actions.iter().any(|a| {
                a.action_id.starts_with("aa_")
                    && (a.block_id.as_deref().and_then(ControlRef::parse).is_some()
                        || a.value.as_deref().and_then(ControlRef::parse).is_some())
            }),
            InteractionKind::ViewSubmission => matches!(
                interaction.callback_id.as_deref(),
                Some(card::REVISE_MODAL) | Some(card::FILL_MODAL)
            ),
            _ => false,
        }
    }

    /// Handle an owner's card click or modal submission (already authorized
    /// by `owner::admit`). Returns `false` when it is not an approval
    /// interaction.
    pub async fn handle_interaction(&self, interaction: &Interaction) -> bool {
        if !self.handles(interaction) {
            return false;
        }
        match interaction.kind {
            InteractionKind::BlockActions => self.block_action(interaction).await,
            InteractionKind::ViewSubmission => self.view_submission(interaction).await,
            _ => {}
        }
        true
    }

    async fn block_action(&self, i: &Interaction) {
        let Some(action) = i.actions.iter().find(|a| a.action_id.starts_with("aa_")) else {
            return;
        };
        let Some(control) = action
            .block_id
            .as_deref()
            .and_then(ControlRef::parse)
            .or_else(|| action.value.as_deref().and_then(ControlRef::parse))
        else {
            return;
        };
        let clicked_at = action
            .action_ts
            .as_deref()
            .and_then(slack_ts_ms)
            .unwrap_or_else(|| self.now());
        let late = self.now() - clicked_at > TRIGGER_TTL_MS;
        let to = ReplyTo {
            channel: i
                .channel_id
                .clone()
                .unwrap_or_else(|| self.config.channel.clone()),
            user: i.user_id.clone(),
            fresh: late,
        };
        let id = control.action_id.as_str();
        let r = card::short_ref(id);
        if late {
            info!(
                action_id = id,
                late_ms = self.now() - clicked_at,
                "slack approvals: late click"
            );
        }
        let verb = match action.action_id.as_str() {
            card::APPROVE => Verb::Approve,
            card::SKIP => Verb::Skip,
            card::RECOMPOSE => Verb::Recompose,
            card::REFINE => {
                let preset = action
                    .selected_option
                    .as_ref()
                    .and_then(|o| o.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Verb::Refine(preset)
            }
            card::REVISE | card::FILL => {
                self.open_redraft_modal(i, &control, action.action_id == card::FILL, &to)
                    .await;
                return;
            }
            other => {
                debug!(action = other, "slack approvals: unknown control");
                return;
            }
        };
        if !matches!(verb, Verb::Recompose) && self.draft_changed(&control) {
            self.reply(&to, STALE_DRAFT_REPLY, None).await;
            self.redraw_own(id, None).await;
            return;
        }
        let outcome = self.decide(id, &verb).await;
        let (mut text, recompose) = Self::outcome_reply(&outcome);
        if late {
            text = format!("{text}\n_(Your click on `{r}` reached me late — the Mac may have been asleep — so this answer is a new message.)_");
        }
        self.reply(&to, &text, recompose.then_some(id)).await;
    }

    /// The control was drawn for a draft that is no longer the action's.
    fn draft_changed(&self, control: &ControlRef) -> bool {
        let Some(shown) = control.digest.as_deref() else {
            return false;
        };
        match self.load(&control.action_id) {
            Some(row) if row.action.status == "pending" => {
                card::draft_digest(row.action.draft_body.as_deref().unwrap_or_default()) != shown
            }
            _ => false,
        }
    }

    async fn open_redraft_modal(
        &self,
        i: &Interaction,
        control: &ControlRef,
        fill: bool,
        to: &ReplyTo,
    ) {
        let id = control.action_id.as_str();
        let r = card::short_ref(id);
        let Some(row) = self.load(id) else {
            self.reply(to, &describe(&ApprovalActionOutcome::NotFound), None)
                .await;
            return;
        };
        if row.action.status != "pending" {
            let outcome = ApprovalActionOutcome::AlreadyResolved {
                status: row.action.status.clone(),
                detail: row.action.error_message.clone(),
            };
            let (text, recompose) = Self::outcome_reply(&outcome);
            self.reply(to, &text, recompose.then_some(id)).await;
            self.redraw_own(id, None).await;
            return;
        }
        if self.draft_changed(control) {
            self.reply(to, STALE_DRAFT_REPLY, None).await;
            self.redraw_own(id, None).await;
            return;
        }
        let draft = row.action.draft_body.clone().unwrap_or_default();
        let needs = split_needs_input(&draft).1;
        if fill && needs.is_empty() {
            self.reply(to, "Nothing left to fill in for this draft.", None)
                .await;
            return;
        }
        let fallback = if fill {
            format!(
                "The missing-info form could not open because the click reached me after Slack's 3-second window (the Mac may have been asleep). Click *Provide missing info* again, or reply `revise {r} <the missing details>`."
            )
        } else {
            format!(
                "The Revise form could not open because the click reached me after Slack's 3-second window (the Mac may have been asleep). Click *Revise* again, or reply `revise {r} <what to change>`."
            )
        };
        let (Some(trigger), false) = (i.trigger_id.as_deref(), to.fresh) else {
            self.reply(
                &ReplyTo {
                    channel: to.channel.clone(),
                    user: None,
                    fresh: true,
                },
                &fallback,
                None,
            )
            .await;
            return;
        };
        let ctx = ModalContext {
            action_id: id.to_string(),
            digest: card::draft_digest(&draft),
            channel: i.channel_id.clone(),
            ts: i.message_ts.clone(),
        };
        let view = if fill {
            card::fill_modal(&ctx, &needs)
        } else {
            card::revise_modal(&ctx, &row.email.subject, &draft)
        };
        if let Err(e) = self.web.open_modal(trigger, view).await {
            warn!(action_id = id, "slack approvals: views.open failed: {e}");
            self.reply(
                &ReplyTo {
                    channel: to.channel.clone(),
                    user: None,
                    fresh: true,
                },
                &fallback,
                None,
            )
            .await;
        }
    }

    async fn view_submission(&self, i: &Interaction) {
        let Some(view) = i.view.as_ref() else {
            return;
        };
        let Some(ctx) = view
            .get("private_metadata")
            .and_then(Value::as_str)
            .and_then(|m| serde_json::from_str::<ModalContext>(m).ok())
        else {
            warn!("slack approvals: modal submission without its context");
            return;
        };
        let to = ReplyTo {
            channel: ctx
                .channel
                .clone()
                .unwrap_or_else(|| self.config.channel.clone()),
            user: i.user_id.clone(),
            fresh: false,
        };
        let feedback = if i.callback_id.as_deref() == Some(card::FILL_MODAL) {
            let needs = self
                .load(&ctx.action_id)
                .and_then(|a| a.action.draft_body)
                .map(|d| split_needs_input(&d).1)
                .unwrap_or_default();
            let filled: Vec<_> = needs
                .into_iter()
                .take(5)
                .enumerate()
                .filter_map(|(n, ask)| {
                    card::view_value(view, &format!("ask_{n}"), "value").map(|v| (ask, v))
                })
                .collect();
            if filled.is_empty() {
                self.reply(&to, NO_VALUES_REPLY, None).await;
                return;
            }
            fill_feedback(&filled)
        } else {
            match card::view_value(view, "feedback", "feedback") {
                Some(f) => f,
                None => {
                    self.reply(&to, "No feedback given — draft unchanged.", None)
                        .await;
                    return;
                }
            }
        };
        let outcome = self.decide(&ctx.action_id, &Verb::Revise(feedback)).await;
        let (text, recompose) = Self::outcome_reply(&outcome);
        self.reply(&to, &text, recompose.then_some(ctx.action_id.as_str()))
            .await;
    }

    // -----------------------------------------------------------------
    // Text commands
    // -----------------------------------------------------------------

    /// The text-command equivalent of every control, with an explicit
    /// action reference: `approve <ref>`, `skip <ref>`, `revise <ref> <what
    /// to change>`, `refine <ref> <preset>`, `recompose <ref>`, and
    /// `approvals` for the pending queue. `None` when `text` is not an
    /// approval command (it goes to the agent instead).
    pub async fn handle_command(&self, text: &str) -> Option<String> {
        let t = text.trim().trim_start_matches(['!', '/']).trim();
        let (word, rest) = match t.split_once(char::is_whitespace) {
            Some((w, r)) => (w, r.trim()),
            None => (t, ""),
        };
        let word = word.to_ascii_lowercase();
        if rest.is_empty() && matches!(word.as_str(), "approvals" | "pending") {
            return Some(self.queue());
        }
        let known = matches!(
            word.as_str(),
            "approve"
                | "send"
                | "skip"
                | "reject"
                | "decline"
                | "revise"
                | "edit"
                | "refine"
                | "recompose"
        );
        if !known {
            return None;
        }
        let (reference, arg) = match rest.split_once(char::is_whitespace) {
            Some((r, a)) => (r, a.trim()),
            None => (rest, ""),
        };
        if reference.is_empty() {
            if matches!(
                word.as_str(),
                "approve" | "skip" | "revise" | "refine" | "recompose"
            ) {
                return Some(format!(
                    "Which approval? Reply `{word} <ref>` with the ref printed on its card.\n\n{}",
                    self.queue()
                ));
            }
            return None;
        }
        // Only something that looks like a reference is one; `send the
        // report` is a request for the agent, not an action.
        let looks_like_ref = reference.len()
            >= augmentagent_store::approval_cards::MIN_ACTION_REF_LEN
            && reference.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
        if !looks_like_ref {
            return None;
        }
        let action_id = match self.store.resolve_action_ref(reference) {
            Ok(ActionRefMatch::One(id)) => id,
            Ok(ActionRefMatch::None) => {
                return Some(format!(
                    "No approval matches `{reference}`. Reply `approvals` to list the pending ones."
                ))
            }
            Ok(ActionRefMatch::Ambiguous(ids)) => {
                let refs: Vec<String> = ids.iter().map(|i| format!("`{i}`")).collect();
                return Some(format!(
                    "`{reference}` matches more than one approval ({}). Use more of the ref.",
                    refs.join(", ")
                ));
            }
            Err(e) => {
                warn!("slack approvals: action lookup failed: {e}");
                return Some(
                    "Could not look that approval up; the details are in the daemon log.".into(),
                );
            }
        };
        let verb = match word.as_str() {
            "approve" | "send" => Verb::Approve,
            "skip" | "reject" | "decline" => Verb::Skip,
            "recompose" => Verb::Recompose,
            "revise" | "edit" => {
                if arg.is_empty() {
                    return Some(format!(
                        "Say what to change: `revise {} <what to change>`.",
                        card::short_ref(&action_id)
                    ));
                }
                Verb::Revise(arg.to_string())
            }
            _ => {
                let preset = arg.to_ascii_lowercase();
                if !PRESETS.iter().any(|p| p.id == preset) {
                    let ids: Vec<String> = PRESETS.iter().map(|p| format!("`{}`", p.id)).collect();
                    return Some(format!("Refine with one of: {}.", ids.join(", ")));
                }
                Verb::Refine(preset)
            }
        };
        let outcome = self.decide(&action_id, &verb).await;
        let (text, _) = Self::outcome_reply(&outcome);
        let recover = match &outcome {
            ApprovalActionOutcome::AlreadyResolved { status, detail }
                if offers_recompose(status, detail.as_deref()) =>
            {
                format!(
                    "\nReply `recompose {}` to restore the draft.",
                    card::short_ref(&action_id)
                )
            }
            _ => String::new(),
        };
        Some(format!("{text}{recover}"))
    }

    /// The pending queue with each approval's reference, as Markdown (text
    /// command answers go through the outbox's Markdown → mrkdwn step).
    fn queue(&self) -> String {
        let rows = self.store.oldest_pending_actions(20).unwrap_or_default();
        if rows.is_empty() {
            return "No approvals are pending.".into();
        }
        // Markdown: the outbox converts it to mrkdwn (and escapes) once.
        let mut s = format!("**Pending approvals** ({})\n", rows.len());
        for (id, from, subject, age_ms) in rows {
            let hours = age_ms / 3_600_000;
            let age = if hours >= 24 {
                format!("{}d", hours / 24)
            } else if hours >= 1 {
                format!("{hours}h")
            } else {
                format!("{}m", age_ms / 60_000)
            };
            s.push_str(&format!(
                "• `{}` — {from} — {subject} · {age}\n",
                card::short_ref(&id)
            ));
        }
        s.push_str("Reply `approve <ref>`, `skip <ref>` or `revise <ref> <what to change>`.");
        s
    }
}

#[async_trait]
impl ApprovalBroker for SlackApprovals {
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        // The reminder carousel prefixes its header to the draft.
        let note = draft
            .lines()
            .next()
            .filter(|l| l.starts_with("🔔 "))
            .map(str::to_string);
        self.post_card(action_id, email, draft, note.as_deref())
            .await
    }

    async fn post_approval_card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        _redraft_count: u32,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.post_card(action_id, email, draft, None).await?;
        // Slack messages have no Discord ids to hand back.
        Ok(None)
    }

    async fn post_flag_notice(&self, email: &Email, reason: &str) -> Result<(), ApprovalError> {
        use crate::delivery::mrkdwn::escape;
        let text = format!(
            "🚩 *Important* — from `{}`\n*{}*\n_reason: {}_",
            escape(&email.from),
            escape(&email.subject),
            escape(reason)
        );
        self.web
            .post_message(PostMessage {
                channel: self.config.channel.clone(),
                text,
                unfurl_links: Some(false),
                link_names: Some(false),
                ..PostMessage::default()
            })
            .await
            .map(|_| ())
            .map_err(|e| ApprovalError::Discord(format!("slack chat.postMessage: {e}")))
    }

    async fn post_digest(&self, title: &str, body: &str) -> Result<(), ApprovalError> {
        let text = format!(
            "📰 *{}*\n{}",
            crate::delivery::mrkdwn::escape(title),
            crate::delivery::markdown_to_mrkdwn(body)
        );
        self.web
            .post_message(PostMessage {
                channel: self.config.channel.clone(),
                text,
                unfurl_links: Some(false),
                link_names: Some(false),
                ..PostMessage::default()
            })
            .await
            .map(|_| ())
            .map_err(|e| ApprovalError::Discord(format!("slack chat.postMessage: {e}")))
    }
}

#[async_trait]
impl ApprovalCardSurface for SlackApprovals {
    fn surface_name(&self) -> &'static str {
        SURFACE
    }

    async fn redraw_cards(&self, action_id: &str, origin: &str) {
        debug!(
            action_id,
            origin, "slack approvals: redraw after a decision elsewhere"
        );
        self.redraw_own(action_id, None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_timestamps_parse_to_milliseconds() {
        assert_eq!(slack_ts_ms("1700000000.123456"), Some(1_700_000_000_123));
        assert_eq!(slack_ts_ms("1700000000"), Some(1_700_000_000_000));
        assert_eq!(slack_ts_ms("x"), None);
    }
}
