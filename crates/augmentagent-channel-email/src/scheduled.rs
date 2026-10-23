//! #500 — ScheduledSendEngine: fires approved-for-later email sends when due.
//!
//! Counterpart of `ScheduledPostEngine` (channel-core/engagement.rs) for the
//! `actions` table, with stronger once-only semantics: every fire is gated by
//! a CAS claim (`scheduled → sending`) before the Composio call, because for
//! email a duplicate send is worse than a stuck row. The other half of that
//! bargain: rows found stuck in `sending` (daemon died mid-send) are flipped
//! to a retry-exempt `error` and surfaced to the owner — never auto-resent,
//! since the crash window includes "Composio accepted the send and we died
//! before recording it".
//!
//! No engine-level send retries: `ComposioClient::execute` already retries
//! 429/5xx/transport errors internally with backoff, so an error surfacing
//! here is either deterministic (deleted draft, revoked auth) or a genuine
//! outage — neither heals inside one tick, and in-tick sleeps would stall
//! every later due row. A failure is terminal for the schedule and the owner
//! gets an honest notice.
//!
//! Zero LLM calls per tick (#448): pure clock/DB/Composio.

use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::{ApprovalBroker, CardSurfaces};
use augmentagent_store::{Store, TriageResult};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::gmail::GmailApi;

/// Default engine cadence. Overridable via
/// `AUGMENTAGENT_SCHEDULED_SEND_INTERVAL_SECS` (wired in the serve arm).
pub const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// Hard wall-clock bound on one `send_draft` round-trip. `ComposioClient` is
/// built on `reqwest::Client::new()` (no request timeout), so without this a
/// hung connection would stall the tick loop forever — and, worse, outlive
/// the stuck-claim grace below, letting the reconcile flip a row whose send
/// is still in flight. Keeping this well under `STUCK_SENDING_GRACE_MS` is
/// what makes that flip sound. Shared with the Approve path in the CLI.
pub const SEND_DRAFT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a row may sit in the `sending` claim before the engine treats it
/// as a crashed send. Must comfortably exceed [`SEND_DRAFT_TIMEOUT`] plus the
/// Composio client's internal retry backoff, so a live in-flight send can
/// never be flipped under its caller.
const STUCK_SENDING_GRACE_MS: i64 = 10 * 60 * 1000;

/// `retryCount` stamp that keeps a row out of `list_retryable_replies` for
/// any plausible `retry_max_attempts` configuration. The generic retry path
/// re-dispatches through `dispatch_reply`, which would repost an approval
/// card for a send that may have actually landed — the exact double-send this
/// engine exists to avoid.
pub const RETRY_EXEMPT_RETRY_COUNT: i64 = 9_999;

/// Sends fired per tick, so a backlog burst can't monopolize the loop. A
/// truncated batch is logged and the remainder fires on the next tick (60s
/// later) — never silently dropped.
const PER_TICK_LIMIT: i64 = 10;

/// Record a daemon-side send in `self_sent_messages`, tolerating a missing
/// provider id (the observer then falls back to thread-proximity matching).
/// Every daemon send path must funnel through here or the outbound observer
/// misreads the send as a manual user reply, supersedes the thread's drafts,
/// and suppresses future ones (#449). Best-effort by construction — the mail
/// is already out; bookkeeping failure is logged loudly, never surfaced as a
/// send failure. Shared by the engine and the CLI's Approve path.
pub fn record_self_send(
    store: &Store,
    sent_message_id: Option<&str>,
    thread_id: Option<&str>,
    entity_id: Option<&str>,
    action_id: Option<&str>,
) {
    let Some(mid) = sent_message_id.filter(|s| !s.is_empty()) else {
        return;
    };
    if let Err(e) = store.record_self_sent_message(mid, thread_id, entity_id, action_id) {
        warn!(
            message_id = mid,
            "failed to record self-sent message; the outbound observer may \
             misread this send as a manual user reply (#449): {e:#}"
        );
    }
}

/// #1291 — how late a scheduled send may still fire. A row found due
/// later than this (the Mac was asleep, the daemon was down) is not sent:
/// it goes back to the approval queue and the owner is told. Overridable
/// with [`MISSED_WINDOW_ENV`].
pub const DEFAULT_MISSED_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Seconds, or `off` to always fire late sends (the pre-#1291 behavior).
pub const MISSED_WINDOW_ENV: &str = "AUGMENTAGENT_SCHEDULED_SEND_MISSED_WINDOW_SECS";

/// The smallest window accepted: two default ticks, so a send is never
/// returned to the queue merely because it was seen on the next tick.
pub const MIN_MISSED_WINDOW: Duration = Duration::from_secs(120);

/// The missed-schedule window from [`MISSED_WINDOW_ENV`]'s value: unset or
/// unreadable → [`DEFAULT_MISSED_WINDOW`]; `off` → `None` (fire however
/// late); a number of seconds, at least [`MIN_MISSED_WINDOW`].
pub fn missed_window_from(raw: Option<&str>) -> Option<Duration> {
    let raw = raw.map(str::trim).unwrap_or_default();
    if raw.eq_ignore_ascii_case("off") {
        return None;
    }
    match raw.parse::<u64>() {
        Ok(secs) => Some(Duration::from_secs(secs).max(MIN_MISSED_WINDOW)),
        Err(_) => {
            if !raw.is_empty() {
                warn!("{MISSED_WINDOW_ENV}=`{raw}` is not a number of seconds or `off`; using the default");
            }
            Some(DEFAULT_MISSED_WINDOW)
        }
    }
}

/// What a platform sender did with a due scheduled row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformFire {
    /// Delivered (the platform recorded it as sent).
    Sent,
    /// Claimed and attempted; the send failed. The row is `error` and the
    /// platform's own retry applies. The message is for the owner.
    Failed(String),
    /// Refused before anything was claimed (no connection, no identity,
    /// nowhere to send): the row is still `scheduled`.
    NotStarted(String),
    /// Someone else resolved or re-armed the row first; nothing was sent.
    LostClaim,
}

/// #1291 — sends scheduled rows of platforms the Gmail path does not own
/// (Slack contact messages). It must claim with the due-gated claim and use
/// the same destination, identity and send log as an immediate send.
#[async_trait::async_trait]
pub trait ScheduledPlatformSender: Send + Sync {
    /// True for the `emails.platform` values this sender owns.
    fn handles(&self, platform: &str) -> bool;

    /// Claim `action_id` if it is due at `now_ms` and send it once.
    async fn fire_due(&self, action_id: &str, now_ms: i64) -> PlatformFire;
}

/// One tick's outcome, for logs and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickSummary {
    /// #1291 — due rows found later than the missed-schedule window (or
    /// whose platform could not start the send) and put back in the queue.
    pub returned_to_queue: usize,
    /// Due rows sent successfully.
    pub fired: usize,
    /// Due rows flipped to retry-exempt error.
    pub failed: usize,
    /// Due rows cancelled at fire time because the owner replied on the
    /// thread after arming the schedule.
    pub superseded: usize,
    /// Rows found stuck in the `sending` claim and flagged for the owner.
    pub stuck_flagged: usize,
}

pub struct ScheduledSendEngine<G: GmailApi> {
    store: Arc<Store>,
    gmail: Arc<G>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
    tick: Duration,
    /// #1291 — `None`: fire however late.
    missed_window: Option<Duration>,
    /// #1291 — senders for platforms other than Gmail.
    platforms: Vec<Arc<dyn ScheduledPlatformSender>>,
    /// #1291 — card surfaces redrawn after the engine moves a row.
    surfaces: Option<CardSurfaces>,
}

impl<G: GmailApi> ScheduledSendEngine<G> {
    pub fn new(
        store: Arc<Store>,
        gmail: Arc<G>,
        broker: Arc<dyn ApprovalBroker>,
        dry_run: bool,
    ) -> Self {
        Self {
            store,
            gmail,
            broker,
            dry_run,
            tick: DEFAULT_TICK,
            missed_window: Some(DEFAULT_MISSED_WINDOW),
            platforms: Vec::new(),
            surfaces: None,
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// #1291 — the missed-schedule window (`None`: fire however late).
    pub fn with_missed_window(mut self, window: Option<Duration>) -> Self {
        self.missed_window = window;
        self
    }

    /// #1291 — add a sender for non-Gmail platforms.
    pub fn with_platform_sender(mut self, sender: Arc<dyn ScheduledPlatformSender>) -> Self {
        self.platforms.push(sender);
        self
    }

    /// #1291 — redraw these surfaces' cards whenever the engine moves a row.
    pub fn with_card_surfaces(mut self, surfaces: CardSurfaces) -> Self {
        self.surfaces = Some(surfaces);
        self
    }

    /// Long-running loop. Exits cleanly on `shutdown`. Same select shape as
    /// every other channel runner.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            tick_secs = self.tick.as_secs(),
            dry_run = self.dry_run,
            "scheduled-send engine started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("scheduled-send engine: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.tick_once(now_ms()).await {
                        Ok(s) if s == TickSummary::default() => {}
                        Ok(s) => info!(
                            fired = s.fired,
                            failed = s.failed,
                            superseded = s.superseded,
                            stuck_flagged = s.stuck_flagged,
                            "scheduled-send tick"
                        ),
                        Err(e) => error!("scheduled-send tick failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One pass: reconcile stuck claims, then fire due rows. Public and
    /// now-parameterized so tests drive it without the timer.
    ///
    /// Dry-run gates EVERYTHING — a `serve --dry-run true` must not mutate
    /// rows, send mail, or post notices; it only reports what it would do.
    pub async fn tick_once(&self, now_ms: i64) -> anyhow::Result<TickSummary> {
        let mut summary = TickSummary::default();

        if self.dry_run {
            for action_id in self
                .store
                .stuck_sending_actions(now_ms, STUCK_SENDING_GRACE_MS)?
            {
                info!("[scheduled-send:dry-run] would flag stuck mid-send row {action_id}");
            }
            for (action_id, scheduled_at_ms, ..) in
                self.store.due_scheduled_actions(now_ms, PER_TICK_LIMIT)?
            {
                if self
                    .missed_window
                    .is_some_and(|w| now_ms - scheduled_at_ms > w.as_millis() as i64)
                {
                    info!("[scheduled-send:dry-run] would return missed {action_id} to the queue");
                    continue;
                }
                // The receipt line the verify gate quotes — keep the exact
                // prefix stable.
                info!("[scheduled-send:dry-run] would fire {action_id}");
            }
            return Ok(summary);
        }

        summary.stuck_flagged = self.reconcile_stuck_sending(now_ms).await?;

        let due = self
            .store
            .due_scheduled_actions(now_ms, PER_TICK_LIMIT)?;
        if due.len() as i64 == PER_TICK_LIMIT {
            info!(
                limit = PER_TICK_LIMIT,
                "scheduled-send: due batch hit the per-tick limit; \
                 remainder fires next tick"
            );
        }
        for (action_id, scheduled_at_ms, armed_at_ms, thread_id) in due {
            // The owner replied after arming: cancelled, whatever else.
            match self
                .supersede_if_replied(&action_id, armed_at_ms, thread_id.as_deref())
                .await
            {
                Ok(Some(FireOutcome::Superseded)) => {
                    summary.superseded += 1;
                    continue;
                }
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(e) => {
                    warn!(action_id, "scheduled-send: reply check errored: {e:#}");
                    continue;
                }
            }
            // #1291 — the missed-schedule policy: a send found later than
            // the window (the host was asleep or the daemon down at the
            // fire time) is not sent late; it goes back to the queue.
            if let Some(window) = self.missed_window {
                let late_ms = now_ms - scheduled_at_ms;
                if late_ms > window.as_millis() as i64 {
                    let why = format!(
                        "it was due {} but this computer was asleep or the daemon was not \
                         running until {} later",
                        when(scheduled_at_ms),
                        human_duration(late_ms)
                    );
                    match self.return_to_queue(&action_id, &why).await {
                        Ok(true) => summary.returned_to_queue += 1,
                        Ok(false) => {}
                        Err(e) => {
                            warn!(action_id, "scheduled-send: return to queue errored: {e:#}")
                        }
                    }
                    continue;
                }
            }
            if let Some(platform) = self.platform_for(&action_id) {
                match self.fire_platform(&platform, &action_id, now_ms).await {
                    Ok(FireOutcome::Sent) => summary.fired += 1,
                    Ok(FireOutcome::Failed) => summary.failed += 1,
                    Ok(FireOutcome::Returned) => summary.returned_to_queue += 1,
                    Ok(FireOutcome::Superseded) => summary.superseded += 1,
                    Ok(FireOutcome::LostClaim) => {}
                    Err(e) => warn!(action_id, "scheduled-send: platform fire errored: {e:#}"),
                }
                continue;
            }
            match self.fire_one(&action_id, scheduled_at_ms, now_ms).await {
                Ok(FireOutcome::Sent) => summary.fired += 1,
                Ok(FireOutcome::Failed) => summary.failed += 1,
                Ok(FireOutcome::Superseded) => summary.superseded += 1,
                Ok(FireOutcome::Returned) => summary.returned_to_queue += 1,
                Ok(FireOutcome::LostClaim) => {}
                Err(e) => {
                    // Bookkeeping error, not a send error: log and move on;
                    // the row stays claimable next tick.
                    warn!(action_id, "scheduled-send: fire errored: {e:#}");
                }
            }
        }
        Ok(summary)
    }

    /// #1291 — the sender that owns this row's platform, if it is not a
    /// Gmail row.
    fn platform_for(&self, action_id: &str) -> Option<Arc<dyn ScheduledPlatformSender>> {
        if self.platforms.is_empty() {
            return None;
        }
        let platform = self
            .store
            .get_action_with_email(action_id)
            .ok()
            .flatten()?
            .email
            .platform;
        self.platforms
            .iter()
            .find(|p| p.handles(&platform))
            .cloned()
    }

    /// #1291 — a due row of a non-Gmail platform: its sender claims (due-
    /// gated) and sends it; the engine does the owner-facing follow-up the
    /// Gmail path does (notice retired, failure told, cards redrawn).
    async fn fire_platform(
        &self,
        sender: &Arc<dyn ScheduledPlatformSender>,
        action_id: &str,
        now_ms: i64,
    ) -> anyhow::Result<FireOutcome> {
        match sender.fire_due(action_id, now_ms).await {
            PlatformFire::Sent => {
                self.delete_notice(action_id).await;
                info!(action_id, "scheduled-send: fired on schedule");
                Ok(FireOutcome::Sent)
            }
            PlatformFire::Failed(msg) => {
                warn!(action_id, "scheduled-send: send failed: {msg}");
                if let Ok(Some(a)) = self.store.get_action_with_email(action_id) {
                    let _ = self
                        .broker
                        .post_flag_notice(&a.email, &format!("Scheduled send failed: {msg}"))
                        .await;
                }
                self.delete_notice(action_id).await;
                Ok(FireOutcome::Failed)
            }
            PlatformFire::NotStarted(msg) => {
                let why = format!("the send could not start: {msg}");
                Ok(if self.return_to_queue(action_id, &why).await? {
                    FireOutcome::Returned
                } else {
                    FireOutcome::LostClaim
                })
            }
            PlatformFire::LostClaim => Ok(FireOutcome::LostClaim),
        }
    }

    /// #1291 — put a due row back in the approval queue without sending it:
    /// the #501 back-to-queue order (repost the card while the row is still
    /// `scheduled`, then the CAS, rolling the repost back when it loses),
    /// the old notice retired, the owner told why, every card redrawn.
    /// `false` when something else resolved the row first.
    async fn return_to_queue(&self, action_id: &str, why: &str) -> anyhow::Result<bool> {
        let Some(action) = self.store.get_action_with_email(action_id)? else {
            return Ok(false);
        };
        let notice = self.store.action_notice(action_id).ok().flatten();
        let draft = augmentagent_approval_discord::append_envelope_markers(
            action.action.draft_body.clone().unwrap_or_default(),
            Some(self.store.as_ref()),
            action_id,
            &action.email.from,
            None,
        );
        let count = self.store.redraft_count(action_id).unwrap_or(0).max(0) as u32;
        let (reposted, repost_failed) = match self
            .broker
            .post_approval_card(action_id, &action.email, &draft, count)
            .await
        {
            Ok(ids) => (ids, false),
            Err(e) => {
                warn!(action_id, "scheduled-send: card repost failed: {e}");
                (None, true)
            }
        };
        if !self
            .store
            .unschedule_action(action_id, "scheduled-send-engine")?
        {
            if let Some((c, m)) = reposted {
                let _ = self.broker.delete_message(c, m).await;
            }
            self.redraw(action_id).await;
            return Ok(false);
        }
        if repost_failed {
            // No card is visible: let the nudge tick post one now.
            let _ = self.store.record_nudge(action_id, now_ms());
        }
        if let Some((chan, msg)) = notice {
            if let (Ok(c), Ok(m)) = (chan.parse::<u64>(), msg.parse::<u64>()) {
                if let Err(e) = self.broker.delete_message(c, m).await {
                    warn!(action_id, "scheduled-send: notice delete failed: {e}");
                }
            }
        }
        let text = format!(
            "Scheduled send was not sent — {why}. It is back in the queue: approve it to send \
             it now, or schedule it again."
        );
        let _ = self.broker.post_flag_notice(&action.email, &text).await;
        self.redraw(action_id).await;
        info!(action_id, "scheduled-send: returned to the queue ({why})");
        Ok(true)
    }

    /// #1291 — redraw the action's cards on every registered surface but
    /// Discord, which retires its scheduled notice by its stored pointer
    /// (and whose approval card was taken down when the send was armed).
    async fn redraw(&self, action_id: &str) {
        if let Some(surfaces) = &self.surfaces {
            surfaces.redraw_except("discord", action_id).await;
        }
    }

    /// Flip rows stuck in the `sending` claim to a retry-exempt error and
    /// tell the owner the truth: the message may or may not have gone out.
    /// This also covers Approve-path claims orphaned by a crash — same
    /// unknown-delivery window, same policy.
    async fn reconcile_stuck_sending(&self, now_ms: i64) -> anyhow::Result<usize> {
        let stuck = self
            .store
            .stuck_sending_actions(now_ms, STUCK_SENDING_GRACE_MS)?;
        let mut flagged = 0usize;
        for action_id in stuck {
            // #1291 — a Slack contact send has its own ledger: Retry looks
            // in the conversation before posting again.
            let gmail = self
                .store
                .get_action_with_email(&action_id)
                .ok()
                .flatten()
                .is_none_or(|a| !self.platforms.iter().any(|p| p.handles(&a.email.platform)));
            let msg = if gmail {
                "scheduled-send: daemon interrupted mid-send — the \
                 message may or may not have been delivered; check the \
                 thread in Gmail before resending"
            } else {
                "scheduled-send: daemon interrupted mid-send — the \
                 message may or may not have been delivered; Retry send \
                 looks for it in the conversation before posting again"
            };
            if !self.store.finish_send_error(
                &action_id,
                msg,
                Some(RETRY_EXEMPT_RETRY_COUNT),
                "scheduled-send-engine",
            )? {
                continue; // Something else resolved it meanwhile.
            }
            flagged += 1;
            warn!(action_id, "scheduled-send: flagged stuck mid-send row");
            if let Ok(Some(a)) = self.store.get_action_with_email(&action_id) {
                let _ = self.broker.post_flag_notice(&a.email, msg).await;
            }
            self.delete_notice(&action_id).await;
        }
        Ok(flagged)
    }

    /// #501 — best-effort removal of the scheduled-notice Discord message
    /// once its row leaves the scheduled/sending pair (fired or superseded).
    /// Pointers are TEXT columns; a parse failure just means there is nothing
    /// deletable. Cleared afterwards either way so the startup sweep is the
    /// only remaining backstop, never a second delete attempt from here.
    ///
    /// #1291 — every caller is a row leaving `scheduled`, so the cards on
    /// the other surfaces (Slack's notice is its card) are redrawn here too.
    async fn delete_notice(&self, action_id: &str) {
        if let Ok(Some((chan, msg))) = self.store.action_notice(action_id) {
            if let (Ok(c), Ok(m)) = (chan.parse::<u64>(), msg.parse::<u64>()) {
                if let Err(e) = self.broker.delete_message(c, m).await {
                    warn!(action_id, "scheduled-send: notice delete failed: {e}");
                }
            }
            let _ = self.store.clear_action_notice(action_id);
        }
        self.redraw(action_id).await;
    }

    /// Fire-time guard, run for every due row before anything else.
    /// `Some(Superseded)` when it cancelled the row, `Some(LostClaim)` when
    /// the row moved meanwhile, `None` to go on.
    async fn supersede_if_replied(
        &self,
        action_id: &str,
        armed_at_ms: i64,
        thread_id: Option<&str>,
    ) -> anyhow::Result<Option<FireOutcome>> {
        // Fire-time guard: a manual owner reply on the thread SINCE THE
        // SCHEDULE WAS ARMED cancels the send, even if the reconcile sweep's
        // scheduled pass missed it (transient store error, 30-min cadence).
        // Bounded to the arming moment: a quick reply the owner sent BEFORE
        // deliberately arming this schedule is not a cancellation signal.
        // The supersede is per-row and CAS-gated; the thread-wide pending
        // supersede is never used for scheduled rows.
        if let Some(tid) = thread_id {
            if self
                .store
                .thread_has_user_reply_after(tid, armed_at_ms)
                .unwrap_or(false)
            {
                if self.store.mark_scheduled_superseded(
                    action_id,
                    "superseded: you replied on this thread after scheduling",
                    "scheduled-send-engine",
                )? {
                    info!(
                        action_id,
                        thread = tid,
                        "scheduled-send: cancelled at fire time, owner \
                         replied after arming"
                    );
                    // #501 — the supersede leaves the notice pointers intact
                    // (see mark_scheduled_superseded); retire the notice here.
                    self.delete_notice(action_id).await;
                    return Ok(Some(FireOutcome::Superseded));
                }
                // CAS lost — something else resolved the row; treat like a
                // lost claim.
                return Ok(Some(FireOutcome::LostClaim));
            }
        }
        Ok(None)
    }

    async fn fire_one(
        &self,
        action_id: &str,
        scheduled_at_ms: i64,
        now_ms: i64,
    ) -> anyhow::Result<FireOutcome> {

        // The claim: exactly one winner, and DUE-GATED — the tick's due list
        // is a snapshot that can be minutes old behind slow earlier sends,
        // and a back-to-queue + re-schedule in that window re-arms the row
        // with a future fire time. The plain status claim would fire it at
        // the OLD due moment, up to a day early (#501 review). Losing means
        // Cancel / Send Now / supersede / re-arm got there first.
        if !self
            .store
            .claim_due_action_for_send(action_id, now_ms, "scheduled-send-engine")?
        {
            return Ok(FireOutcome::LostClaim);
        }

        // Load the row FRESH, after the claim. An `update-draft` repoint can
        // land between the due query and the claim; a pre-claim snapshot's
        // draftId would then point at a Gmail draft that no longer exists.
        let action = match self.store.get_action_with_email(action_id)? {
            Some(a) => a,
            None => {
                let _ = self.store.finish_send_error(
                    action_id,
                    "scheduled-send: action row disappeared after claim",
                    Some(RETRY_EXEMPT_RETRY_COUNT),
                    "scheduled-send-engine",
                );
                self.delete_notice(action_id).await;
                return Ok(FireOutcome::Failed);
            }
        };
        let (Some(draft_id), Some(entity_id)) = (
            action.draft_id.clone(),
            action.email.account_entity_id.clone(),
        ) else {
            let msg = "scheduled-send: action has no draftId/accountEntityId; \
                       cannot send";
            let _ = self.store.finish_send_error(
                action_id,
                msg,
                Some(RETRY_EXEMPT_RETRY_COUNT),
                "scheduled-send-engine",
            );
            warn!(action_id, "{msg}");
            let _ = self.broker.post_flag_notice(&action.email, msg).await;
            // A dead schedule must not keep advertising "Sends <t>" (#501
            // review) — retire the notice on every failure path too.
            self.delete_notice(action_id).await;
            return Ok(FireOutcome::Failed);
        };

        // Single attempt, hard wall-clock bound. The Composio client retries
        // transients internally; anything surfacing here is terminal for the
        // schedule. A timeout is an UNKNOWN outcome (the send may have
        // landed) — same honest wording as the stuck-claim notice, and
        // retry-exempt so nothing re-sends automatically.
        let sent_id = match tokio::time::timeout(
            SEND_DRAFT_TIMEOUT,
            self.gmail.send_draft(&entity_id, &draft_id),
        )
        .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                let msg = format!(
                    "scheduled send failed — not retried automatically: \
                     send_draft: {e}"
                );
                let _ = self.store.finish_send_error(
                    action_id,
                    &msg,
                    Some(RETRY_EXEMPT_RETRY_COUNT),
                    "scheduled-send-engine",
                );
                let _ = self.broker.post_flag_notice(&action.email, &msg).await;
                self.delete_notice(action_id).await;
                return Ok(FireOutcome::Failed);
            }
            Err(_elapsed) => {
                let msg = format!(
                    "scheduled send timed out after {}s — the message may or \
                     may not have been delivered; check the thread in Gmail \
                     before resending",
                    SEND_DRAFT_TIMEOUT.as_secs()
                );
                let _ = self.store.finish_send_error(
                    action_id,
                    &msg,
                    Some(RETRY_EXEMPT_RETRY_COUNT),
                    "scheduled-send-engine",
                );
                let _ = self.broker.post_flag_notice(&action.email, &msg).await;
                self.delete_notice(action_id).await;
                return Ok(FireOutcome::Failed);
            }
        };

        // Post-send bookkeeping, in run_approve's exact order (#449): the
        // self-send record lands BEFORE the status flip so an observer tick
        // racing this send can never catch the message in SENT without
        // knowing it was ours.
        record_self_send(
            &self.store,
            sent_id.as_deref(),
            action.email.thread_id.as_deref(),
            Some(entity_id.as_str()),
            Some(action_id),
        );
        let _ = self
            .store
            .finish_send_sent(action_id, "scheduled-send-engine");
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        match self.store.record_user_edit_as_tone_example(action_id) {
            Ok(_) => {}
            Err(e) => warn!(
                action_id,
                "scheduled-send: record_user_edit_as_tone_example failed: {e}"
            ),
        }
        // #501 — the send happened; the scheduled notice (Send Now / Back to
        // queue / Cancel) is now stale. Best-effort, after all send
        // bookkeeping: a Discord hiccup here must not shadow a landed send.
        self.delete_notice(action_id).await;
        info!(
            action_id,
            scheduled_at_ms, "scheduled-send: fired on schedule"
        );
        Ok(FireOutcome::Sent)
    }
}

enum FireOutcome {
    Sent,
    Failed,
    Superseded,
    /// #1291 — put back in the approval queue unsent.
    Returned,
    LostClaim,
}

/// A fire time for owner-facing text, in the owner's zone.
fn when(at_ms: i64) -> String {
    augmentagent_approval_discord::timeparse::describe_send_time(
        at_ms,
        &augmentagent_approval_discord::timeparse::owner_zone(),
    )
}

/// `95 minutes` / `3 hours` / `2 days`.
fn human_duration(ms: i64) -> String {
    let mins = (ms / 60_000).max(1);
    if mins < 120 {
        format!("{mins} minutes")
    } else if mins < 48 * 60 {
        format!("{} hours", mins / 60)
    } else {
        format!("{} days", mins / (24 * 60))
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmail::GmailError;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{ApprovalError, NoopBroker};
    use augmentagent_store::{ActionStatus, Email};
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    /// Mock GmailApi: records send_draft calls, returns scripted results.
    struct MockGmail {
        /// Each entry is one scripted send_draft result; calls past the end
        /// of the script succeed with a fresh id.
        script: Mutex<Vec<Result<Option<String>, String>>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl MockGmail {
        fn ok() -> Self {
            Self {
                script: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn scripted(script: Vec<Result<Option<String>, String>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn last_draft_id(&self) -> Option<String> {
            self.calls.lock().unwrap().last().map(|(_, d)| d.clone())
        }
    }

    #[async_trait]
    impl GmailApi for MockGmail {
        async fn fetch_unread(
            &self,
            _entity_id: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }
        async fn fetch_with_query(
            &self,
            _entity_id: &str,
            _query: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }
        async fn create_draft(
            &self,
            _entity_id: &str,
            _to: &str,
            _subject: &str,
            _body: &str,
            _thread_id: Option<&str>,
        ) -> Result<String, GmailError> {
            Ok("draft-new".into())
        }
        async fn send_draft(
            &self,
            entity_id: &str,
            draft_id: &str,
        ) -> Result<Option<String>, GmailError> {
            self.calls
                .lock()
                .unwrap()
                .push((entity_id.to_string(), draft_id.to_string()));
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Ok(Some("sent-id".into()));
            }
            match script.remove(0) {
                Ok(v) => Ok(v),
                Err(msg) => Err(GmailError::Composio { message: msg }),
            }
        }
        async fn delete_draft(
            &self,
            _entity_id: &str,
            _draft_id: &str,
        ) -> Result<(), GmailError> {
            Ok(())
        }
    }

    /// Broker that records flag notices, so failure-path tests can assert
    /// the owner was told.
    struct RecordingBroker {
        notices: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(
            &self,
            _: &str,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _email: &Email,
            reason: &str,
        ) -> Result<(), ApprovalError> {
            self.notices.lock().unwrap().push(reason.to_string());
            Ok(())
        }
    }

    fn test_store() -> (Arc<Store>, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn seed_scheduled(
        store: &Store,
        message_id: &str,
        at_ms: i64,
    ) -> String {
        let email = Email {
            attachments: Vec::new(),
            message_id: message_id.into(),
            thread_id: Some(format!("th-{message_id}")),
            from: "peer@example.com".into(),
            to: String::new(),
            cc: String::new(),
            subject: "hello".into(),
            body: "body".into(),
            date: String::new(),
            account_entity_id: Some("entity-1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&email).unwrap();
        let id = store
            .log_action(
                message_id,
                Some(&format!("th-{message_id}")),
                "peer@example.com",
                "hello",
                None,
                Some("the draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        store.set_action_draft_id(&id, "draft-1").unwrap();
        assert!(store.schedule_action(&id, at_ms, "test").unwrap());
        id
    }

    fn status_of(store: &Store, id: &str) -> String {
        store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status
    }

    #[tokio::test]
    async fn fires_due_row_and_records_self_send_before_flip() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m1", 1_000);
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );

        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(gmail.call_count(), 1, "exactly one send");
        assert_eq!(status_of(&store, &id), "sent");
        // Self-send recorded with the id the mock returned.
        let ids = store.self_sent_message_ids_since(0).unwrap();
        assert!(ids.contains("sent-id"));
    }

    #[tokio::test]
    async fn skips_future_rows() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m2", 10_000);
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0);
        assert_eq!(status_of(&store, &id), "scheduled");
    }

    #[tokio::test]
    async fn dry_run_mutates_nothing_including_stuck_rows() {
        let (store, _f) = test_store();
        let due = seed_scheduled(&store, "m3", 1_000);
        // Also plant a stuck 'sending' row — dry-run must not flip it.
        let stuck = seed_scheduled(&store, "m3b", 1_000);
        store
            .claim_action_for_send(&stuck, ActionStatus::Scheduled, "t")
            .unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            true,
        );
        let s = engine
            .tick_once(now_ms() + STUCK_SENDING_GRACE_MS + 60_000)
            .await
            .unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0);
        assert_eq!(status_of(&store, &due), "scheduled");
        assert_eq!(
            status_of(&store, &stuck),
            "sending",
            "dry-run must not flip stuck rows to error"
        );
        assert!(broker.notices.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn lost_claim_means_no_send() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m4", 1_000);
        assert!(store.cancel_scheduled_action(&id, "", "test").unwrap());
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0, "cancelled row must never send");
        assert_eq!(status_of(&store, &id), "rejected");
    }

    #[tokio::test]
    async fn fire_time_guard_supersedes_on_reply_after_arming_only() {
        let (store, _f) = test_store();
        // Row A: owner replied AFTER arming — must be cancelled.
        let a = seed_scheduled(&store, "m5", 1_000);
        store
            .record_outbound_thread_event(
                "entity-1",
                "reply-after-arm",
                Some("th-m5"),
                now_ms() + 1_000,
            )
            .unwrap();
        // Row B: owner replied BEFORE arming — must still fire. Seed the
        // reply first, then arm (schedule_action stamps status_updated_at
        // after the reply's sent_at_ms).
        store
            .record_outbound_thread_event(
                "entity-1",
                "reply-before-arm",
                Some("th-m6"),
                now_ms() - 60_000,
            )
            .unwrap();
        let b = seed_scheduled(&store, "m6", 1_000);

        let gmail = Arc::new(MockGmail::ok());
        // The fixture's fire times are epoch-relative while the reply times
        // are real, so every row looks decades late: this test is about the
        // reply guard, not the #1291 missed-schedule window.
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        )
        .with_missed_window(None);
        let s = engine.tick_once(now_ms() + 5_000).await.unwrap();
        assert_eq!(s.superseded, 1);
        assert_eq!(s.fired, 1);
        assert_eq!(status_of(&store, &a), "superseded");
        assert_eq!(
            status_of(&store, &b),
            "sent",
            "a reply that PREDATES arming is not a cancellation signal"
        );
    }

    #[tokio::test]
    async fn send_failure_is_terminal_retry_exempt_and_notifies() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m7", 1_000);
        let gmail = Arc::new(MockGmail::scripted(vec![Err("draft gone".into())]));
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.failed, 1);
        assert_eq!(
            gmail.call_count(),
            1,
            "no engine-level retries — the Composio client retries transients \
             internally"
        );
        assert_eq!(status_of(&store, &id), "error");
        let retryable = store
            .list_retryable_replies("gmail", now_ms(), 86_400_000, 0, 5, 10)
            .unwrap();
        assert!(retryable.iter().all(|r| r.action.id != id));
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("not retried automatically"));
    }

    #[tokio::test]
    async fn fire_uses_post_claim_draft_id_after_repoint() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m8", 1_000);
        // Simulate an update-draft repoint landing before the tick: the
        // engine must send the NEW draft id, not a stale snapshot.
        store.set_action_draft_id(&id, "draft-2-repointed").unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(
            gmail.last_draft_id().as_deref(),
            Some("draft-2-repointed"),
            "must load the row fresh after the claim"
        );
    }

    #[tokio::test]
    async fn stuck_sending_rows_are_flagged_never_resent() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m9", 1_000);
        store
            .claim_action_for_send(&id, ActionStatus::Scheduled, "engine")
            .unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            false,
        );
        // Within the grace window: untouched.
        let now = now_ms();
        let s = engine.tick_once(now).await.unwrap();
        assert_eq!(s.stuck_flagged, 0);
        assert_eq!(status_of(&store, &id), "sending");
        // Age the claim past the grace window.
        let s = engine
            .tick_once(now + STUCK_SENDING_GRACE_MS + 60_000)
            .await
            .unwrap();
        assert_eq!(s.stuck_flagged, 1);
        assert_eq!(gmail.call_count(), 0, "NEVER auto-resend an unknown-delivery row");
        assert_eq!(status_of(&store, &id), "error");
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("may or may not have been delivered"));
    }

    // -----------------------------------------------------------------
    // #1291 — missed-schedule policy, platform senders, card redraws
    // -----------------------------------------------------------------

    use augmentagent_approval_discord::ApprovalCardSurface;

    const MIN: i64 = 60_000;

    /// Records every broker call the engine makes.
    #[derive(Default)]
    struct Surfaces {
        flags: Mutex<Vec<String>>,
        reposts: Mutex<Vec<String>>,
        deletes: Mutex<Vec<(u64, u64)>>,
    }

    #[async_trait]
    impl ApprovalBroker for Surfaces {
        async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(&self, _: &Email, reason: &str) -> Result<(), ApprovalError> {
            self.flags.lock().unwrap().push(reason.to_string());
            Ok(())
        }
        async fn post_approval_card(
            &self,
            action_id: &str,
            _: &Email,
            _: &str,
            _: u32,
        ) -> Result<Option<(u64, u64)>, ApprovalError> {
            self.reposts.lock().unwrap().push(action_id.to_string());
            Ok(Some((7, 8)))
        }
        async fn delete_message(&self, c: u64, m: u64) -> Result<(), ApprovalError> {
            self.deletes.lock().unwrap().push((c, m));
            Ok(())
        }
    }

    #[derive(Default)]
    struct Redraws {
        name: &'static str,
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ApprovalCardSurface for Redraws {
        fn surface_name(&self) -> &'static str {
            self.name
        }
        async fn redraw_cards(&self, action_id: &str, _origin: &str) {
            self.seen.lock().unwrap().push(action_id.to_string());
        }
    }

    /// A non-Gmail platform: claims with the due-gated claim like the real
    /// Slack sender and answers what it was scripted to.
    struct FakePlatform {
        store: Arc<Store>,
        answer: Mutex<Option<PlatformFire>>,
        calls: Mutex<Vec<(String, i64)>>,
    }

    #[async_trait]
    impl ScheduledPlatformSender for FakePlatform {
        fn handles(&self, platform: &str) -> bool {
            platform == "slack"
        }
        async fn fire_due(&self, action_id: &str, now_ms: i64) -> PlatformFire {
            self.calls
                .lock()
                .unwrap()
                .push((action_id.to_string(), now_ms));
            let answer = self
                .answer
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(PlatformFire::Sent);
            match &answer {
                PlatformFire::Sent => {
                    assert!(self
                        .store
                        .claim_due_action_for_send(action_id, now_ms, "t")
                        .unwrap());
                    self.store.finish_send_sent(action_id, "t").unwrap();
                }
                PlatformFire::Failed(m) => {
                    assert!(self
                        .store
                        .claim_due_action_for_send(action_id, now_ms, "t")
                        .unwrap());
                    self.store
                        .finish_send_error(action_id, m, None, "t")
                        .unwrap();
                }
                PlatformFire::NotStarted(_) | PlatformFire::LostClaim => {}
            }
            answer
        }
    }

    fn seed_slack_scheduled(store: &Store, message_id: &str, at_ms: i64) -> String {
        let email = Email {
            attachments: Vec::new(),
            message_id: message_id.into(),
            thread_id: Some("C00000009".into()),
            from: "Contact Example <slack:U00000077>".into(),
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            body: "hi".into(),
            date: String::new(),
            account_entity_id: Some("slack:team:T00000009".into()),
            platform: "slack".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&email).unwrap();
        let id = store
            .log_action(
                message_id,
                Some("C00000009"),
                &email.from,
                "",
                None,
                Some("see you then"),
                ActionStatus::Pending,
            )
            .unwrap();
        assert!(store.schedule_action(&id, at_ms, "slack").unwrap());
        id
    }

    struct Rig {
        store: Arc<Store>,
        _f: NamedTempFile,
        gmail: Arc<MockGmail>,
        broker: Arc<Surfaces>,
        platform: Arc<FakePlatform>,
        slack: Arc<Redraws>,
        discord: Arc<Redraws>,
        _keep: Vec<Arc<dyn ApprovalCardSurface>>,
        surfaces: CardSurfaces,
    }

    fn rig() -> Rig {
        let (store, f) = test_store();
        let slack = Arc::new(Redraws {
            name: "slack",
            ..Default::default()
        });
        let discord = Arc::new(Redraws {
            name: "discord",
            ..Default::default()
        });
        let surfaces = CardSurfaces::new();
        let keep: Vec<Arc<dyn ApprovalCardSurface>> = vec![slack.clone(), discord.clone()];
        for k in &keep {
            surfaces.register(k);
        }
        Rig {
            platform: Arc::new(FakePlatform {
                store: Arc::clone(&store),
                answer: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
            }),
            store,
            _f: f,
            gmail: Arc::new(MockGmail::ok()),
            broker: Arc::new(Surfaces::default()),
            slack,
            discord,
            _keep: keep,
            surfaces,
        }
    }

    impl Rig {
        fn engine(&self) -> ScheduledSendEngine<MockGmail> {
            ScheduledSendEngine::new(
                Arc::clone(&self.store),
                Arc::clone(&self.gmail),
                Arc::clone(&self.broker) as Arc<dyn ApprovalBroker>,
                false,
            )
            .with_missed_window(Some(Duration::from_secs(30 * 60)))
            .with_platform_sender(Arc::clone(&self.platform) as Arc<dyn ScheduledPlatformSender>)
            .with_card_surfaces(self.surfaces.clone())
        }
    }

    #[test]
    fn the_missed_window_is_configurable_and_can_be_turned_off() {
        assert_eq!(missed_window_from(None), Some(DEFAULT_MISSED_WINDOW));
        assert_eq!(missed_window_from(Some("")), Some(DEFAULT_MISSED_WINDOW));
        assert_eq!(
            missed_window_from(Some("3600")),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(missed_window_from(Some(" off ")), None);
        assert_eq!(
            missed_window_from(Some("5")),
            Some(MIN_MISSED_WINDOW),
            "clamped"
        );
        assert_eq!(
            missed_window_from(Some("soon")),
            Some(DEFAULT_MISSED_WINDOW)
        );
    }

    /// The host slept through the fire time but woke inside the window:
    /// the send goes out once, late.
    #[tokio::test]
    async fn a_send_found_late_within_the_window_fires_once() {
        let r = rig();
        let at = 10 * MIN;
        let id = seed_scheduled(&r.store, "late-1", at);
        let engine = r.engine();
        let s = engine.tick_once(at + 29 * MIN).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(s.returned_to_queue, 0);
        assert_eq!(r.gmail.call_count(), 1);
        assert_eq!(status_of(&r.store, &id), "sent");
        let again = engine.tick_once(at + 30 * MIN).await.unwrap();
        assert_eq!(again, TickSummary::default());
        assert_eq!(r.gmail.call_count(), 1);
    }

    /// The host slept past the window: nothing is sent, the draft is back
    /// in the queue as an actionable card, the old notice is retired, the
    /// owner is told, and every surface redraws.
    #[tokio::test]
    async fn a_send_found_beyond_the_window_returns_to_the_queue_and_tells_the_owner() {
        let r = rig();
        let at = 10 * MIN;
        let id = seed_scheduled(&r.store, "late-2", at);
        r.store.set_action_notice(&id, "11", "22").unwrap();
        let engine = r.engine();
        let s = engine.tick_once(at + 31 * MIN).await.unwrap();
        assert_eq!(s.returned_to_queue, 1);
        assert_eq!(s.fired, 0);
        assert_eq!(r.gmail.call_count(), 0, "a missed send is never fired late");
        assert_eq!(status_of(&r.store, &id), "pending");
        assert_eq!(r.store.action_scheduled_at(&id).unwrap(), None);
        assert_eq!(r.broker.reposts.lock().unwrap().clone(), vec![id.clone()]);
        assert_eq!(r.broker.deletes.lock().unwrap().clone(), vec![(11, 22)]);
        let flags = r.broker.flags.lock().unwrap().clone();
        assert_eq!(flags.len(), 1);
        assert!(flags[0].contains("was not sent"), "{}", flags[0]);
        assert!(flags[0].contains("back in the queue"), "{}", flags[0]);
        assert_eq!(r.slack.seen.lock().unwrap().clone(), vec![id.clone()]);
        // Nothing fires later either.
        let later = engine.tick_once(at + 60 * MIN).await.unwrap();
        assert_eq!(later, TickSummary::default());
        assert_eq!(r.gmail.call_count(), 0);
    }

    #[tokio::test]
    async fn with_the_window_off_a_late_send_still_fires() {
        let r = rig();
        let id = seed_scheduled(&r.store, "late-3", 10 * MIN);
        let engine = r.engine().with_missed_window(None);
        let s = engine.tick_once(10 * MIN + 24 * 60 * MIN).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(status_of(&r.store, &id), "sent");
    }

    /// A daemon restart between arming and the fire time: the schedule is
    /// in the store, the new engine fires it once, and a second tick (or a
    /// third daemon) finds nothing.
    #[tokio::test]
    async fn a_restart_between_schedule_and_fire_sends_exactly_once() {
        let r = rig();
        let at = 10 * MIN;
        let id = seed_scheduled(&r.store, "restart-1", at);
        let before = r.engine();
        assert_eq!(
            before.tick_once(at - MIN).await.unwrap(),
            TickSummary::default()
        );
        drop(before);
        let path = r._f.path().to_path_buf();
        let reopened = Arc::new(Store::open(&path).unwrap());
        let after = ScheduledSendEngine::new(
            Arc::clone(&reopened),
            Arc::clone(&r.gmail),
            Arc::clone(&r.broker) as Arc<dyn ApprovalBroker>,
            false,
        );
        assert_eq!(after.tick_once(at).await.unwrap().fired, 1);
        assert_eq!(
            after.tick_once(at + MIN).await.unwrap(),
            TickSummary::default()
        );
        let third = ScheduledSendEngine::new(
            Arc::new(Store::open(&path).unwrap()),
            Arc::clone(&r.gmail),
            Arc::clone(&r.broker) as Arc<dyn ApprovalBroker>,
            false,
        );
        assert_eq!(
            third.tick_once(at + 2 * MIN).await.unwrap(),
            TickSummary::default()
        );
        assert_eq!(r.gmail.call_count(), 1);
        assert_eq!(status_of(&reopened, &id), "sent");
    }

    /// A Slack row is handed to its platform sender at the tick's time —
    /// never to Gmail — and the cards on every surface but Discord (which
    /// retires its notice by pointer, as before) are redrawn.
    #[tokio::test]
    async fn a_slack_row_fires_through_its_platform_sender_and_redraws_the_cards() {
        let r = rig();
        let at = 10 * MIN;
        let id = seed_slack_scheduled(&r.store, "slack-1", at);
        r.store.set_action_notice(&id, "11", "22").unwrap();
        let engine = r.engine();
        assert_eq!(
            engine.tick_once(at - 1).await.unwrap(),
            TickSummary::default()
        );
        assert!(r.platform.calls.lock().unwrap().is_empty());
        let s = engine.tick_once(at).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(
            r.platform.calls.lock().unwrap().clone(),
            vec![(id.clone(), at)]
        );
        assert_eq!(r.gmail.call_count(), 0, "a Slack row never reaches Gmail");
        assert_eq!(status_of(&r.store, &id), "sent");
        assert_eq!(r.broker.deletes.lock().unwrap().clone(), vec![(11, 22)]);
        assert_eq!(r.slack.seen.lock().unwrap().clone(), vec![id.clone()]);
        assert!(r.discord.seen.lock().unwrap().is_empty());
        assert_eq!(
            engine.tick_once(at + MIN).await.unwrap(),
            TickSummary::default()
        );
        assert_eq!(r.platform.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_slack_send_that_failed_tells_the_owner_and_that_could_not_start_goes_back() {
        let r = rig();
        let at = 10 * MIN;
        let failed = seed_slack_scheduled(&r.store, "slack-2", at);
        *r.platform.answer.lock().unwrap() =
            Some(PlatformFire::Failed("Slack refused the message".into()));
        let engine = r.engine();
        let s = engine.tick_once(at).await.unwrap();
        assert_eq!(s.failed, 1);
        assert_eq!(status_of(&r.store, &failed), "error");
        assert!(r.broker.flags.lock().unwrap()[0].contains("Slack refused"));

        let blocked = seed_slack_scheduled(&r.store, "slack-3", at);
        *r.platform.answer.lock().unwrap() =
            Some(PlatformFire::NotStarted("no Slack connection".into()));
        let s = engine.tick_once(at + MIN).await.unwrap();
        assert_eq!(s.returned_to_queue, 1);
        assert_eq!(status_of(&r.store, &blocked), "pending");
        let flags = r.broker.flags.lock().unwrap().clone();
        assert!(flags[1].contains("no Slack connection"), "{flags:?}");
        assert!(flags[1].contains("back in the queue"), "{flags:?}");
    }

    /// A Slack row that is missed is returned without reaching its sender.
    #[tokio::test]
    async fn a_missed_slack_send_never_reaches_its_platform_sender() {
        let r = rig();
        let at = 10 * MIN;
        let id = seed_slack_scheduled(&r.store, "slack-4", at);
        let s = r.engine().tick_once(at + 45 * MIN).await.unwrap();
        assert_eq!(s.returned_to_queue, 1);
        assert!(r.platform.calls.lock().unwrap().is_empty());
        assert_eq!(status_of(&r.store, &id), "pending");
    }
}
