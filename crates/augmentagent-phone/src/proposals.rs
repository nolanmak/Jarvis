//! Agent-proposed calls (#1553). A proactive loop may ask, never dial: a
//! proposal is a question posted to the owner's private control chat. Only an
//! authenticated owner `yes` there, inside the 15-minute window, dials, through
//! the same `call_owner` path as `call me`. Rate limits and quiet hours apply
//! when the proposal is recorded; quiet hours defer delivery, never drop it.
use crate::{ingress::ConversationAgent, Error, PhoneService};
use augmentagent_store::phone::{
    CallProposal, NewCallProposal, PhoneCall, PhoneError, PhoneOrigin, ProposalAnswer,
    PHONE_PROPOSAL_AGENDA_MAX, PHONE_PROPOSAL_ITEM_MAX, PHONE_PROPOSAL_REASON_MAX,
};
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};

/// After a window closes, a late `yes`/`no` still gets the fixed "expired"
/// reply for this long instead of reaching the agent.
pub const LATE_REPLY_GRACE_MS: i64 = 15 * 60_000;
pub const DEFAULT_QUIET_HOURS: &str = "22:00-08:00";
pub const EXPIRED_REPLY: &str =
    "That call proposal expired, so I won't call. Say ‘call me’ if you want a call now.";
pub const OTHER_CONVERSATION_REPLY: &str =
    "I asked about the call in our private chat. Answer there, or say ‘call me’ here.";
pub const DECLINED_REPLY: &str = "OK, no call. I'll bring it up another time.";
const NO_PROPOSAL_REPLY: &str = "There's no call proposal waiting for an answer.";

/// Services (one per process in production) with an open answer window.
/// Chat adapters route a bare `yes`/`no` to the phone service only while
/// this is non-zero, so ordinary replies reach the agent the rest of the time.
static OPEN_WINDOWS: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn window_open() -> bool {
    OPEN_WINDOWS.load(Ordering::SeqCst) > 0
}

/// Daily quiet window in owner-local time, `[start, end)`. It may wrap midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietHours {
    start: NaiveTime,
    end: NaiveTime,
}
impl QuietHours {
    /// `HH:MM-HH:MM`, 24-hour clock. Equal start and end is rejected.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let (start, end) = raw.trim().split_once('-').ok_or(Error::NotConfigured)?;
        let time = |s: &str| {
            NaiveTime::parse_from_str(s.trim(), "%H:%M").map_err(|_| Error::NotConfigured)
        };
        let (start, end) = (time(start)?, time(end)?);
        if start == end {
            return Err(Error::NotConfigured);
        }
        Ok(Self { start, end })
    }
    fn contains(&self, t: NaiveTime) -> bool {
        if self.start < self.end {
            self.start <= t && t < self.end
        } else {
            t >= self.start || t < self.end
        }
    }
}

/// Where and when proposals may be delivered.
#[derive(Clone, Debug)]
pub struct ProposalPolicy {
    quiet: Option<QuietHours>,
    /// Owner-local zone; `None` is the host's local time.
    zone: Option<chrono_tz::Tz>,
}
impl Default for ProposalPolicy {
    fn default() -> Self {
        Self {
            quiet: QuietHours::parse(DEFAULT_QUIET_HOURS).ok(),
            zone: None,
        }
    }
}
impl ProposalPolicy {
    pub fn new(quiet: Option<QuietHours>, zone: Option<chrono_tz::Tz>) -> Self {
        Self { quiet, zone }
    }
    /// `AUGMENTAGENT_PHONE_QUIET_HOURS` (`HH:MM-HH:MM`, or `off`; default
    /// 22:00-08:00) in `AUGMENTAGENT_TIMEZONE` (IANA; default host local).
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        let quiet = match get("AUGMENTAGENT_PHONE_QUIET_HOURS").as_deref().map(str::trim) {
            None | Some("") => QuietHours::parse(DEFAULT_QUIET_HOURS).ok(),
            Some("off") => None,
            Some(raw) => Some(QuietHours::parse(raw)?),
        };
        let zone = match get("AUGMENTAGENT_TIMEZONE").as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(raw.parse().map_err(|_| Error::NotConfigured)?),
        };
        Ok(Self { quiet, zone })
    }
    /// `now_ms`, or the first instant after it that is outside quiet hours.
    pub fn delivery_ms(&self, now_ms: i64) -> i64 {
        match self.zone {
            Some(zone) => self.delivery_in(&zone, now_ms),
            None => self.delivery_in(&chrono::Local, now_ms),
        }
    }
    fn delivery_in<Z: TimeZone>(&self, zone: &Z, now_ms: i64) -> i64 {
        let Some(quiet) = self.quiet else {
            return now_ms;
        };
        let Some(now) = DateTime::<Utc>::from_timestamp_millis(now_ms) else {
            return now_ms;
        };
        let local = now.with_timezone(zone);
        if !quiet.contains(local.time()) {
            return now_ms;
        }
        // The window ends today or, when it has not yet ended, tomorrow.
        let mut date = local.date_naive();
        if local.time() >= quiet.end {
            date += Duration::days(1);
        }
        // A DST gap at the end time moves delivery to the next valid minute.
        for minutes in 0..=120 {
            let candidate = date.and_time(quiet.end) + Duration::minutes(minutes);
            if let Some(at) = zone.from_local_datetime(&candidate).earliest() {
                return at.with_timezone(&Utc).timestamp_millis().max(now_ms);
            }
        }
        now_ms
    }
}

/// What `propose_call` recorded.
#[derive(Debug, Clone)]
pub struct ProposalReceipt {
    pub id: String,
    /// When the question is posted to the owner's chat.
    pub deliver_ms: i64,
    /// True when quiet hours moved delivery later.
    pub deferred: bool,
}

/// The agenda a call carries into its first phone turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallAgenda {
    pub reason: String,
    pub items: Vec<String>,
}
impl CallAgenda {
    fn from_proposal(proposal: &CallProposal) -> Self {
        let items = if proposal.agenda.is_empty() {
            vec![proposal.reason.clone()]
        } else {
            proposal.agenda.clone()
        };
        Self {
            reason: proposal.reason.clone(),
            items,
        }
    }
    /// Prompt section for the call's opening turn. Data, not instructions;
    /// the caller adds its own instruction to open with it.
    pub fn prompt_section(&self) -> String {
        let mut section = format!("Reason: {}", self.reason);
        for item in &self.items {
            section.push_str("\n- ");
            section.push_str(item);
        }
        section
    }
}
/// Instruction placed with the agenda in the first phone turn's prompt.
pub const AGENDA_INSTRUCTION: &str = "You proposed this call and the owner accepted. In your first reply, say briefly that you called about the items below and start with the first one.";

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
fn question(agenda: &CallAgenda) -> String {
    let n = agenda.items.len();
    let mut text = format!(
        "I have {n} thing{} to go over, call now? (yes/no, expires in 15 min)",
        if n == 1 { "" } else { "s" }
    );
    text.push_str("\nReason: ");
    text.push_str(&agenda.reason);
    for item in &agenda.items {
        text.push_str("\n- ");
        text.push_str(item);
    }
    text
}

/// A bare consent reply. Anything else is ordinary chat text.
pub(crate) fn consent(text: &str) -> Option<bool> {
    let reply = text
        .trim()
        .trim_start_matches(['!', '/'])
        .trim_end_matches(['!', '.'])
        .trim()
        .to_ascii_lowercase();
    match reply.as_str() {
        "yes" | "y" | "yes please" | "call now" | "call me now" => Some(true),
        "no" | "n" | "no thanks" | "not now" => Some(false),
        _ => None,
    }
}

impl PhoneService {
    /// Record a call proposal for `owner_id` and schedule its question for
    /// the owner's private control chat. Never dials and never posts here:
    /// [`PhoneService::deliver_call_proposals`] posts it when due.
    pub fn propose_call(
        &self,
        owner_id: &str,
        reason: &str,
        agenda: &[String],
    ) -> Result<ProposalReceipt, Error> {
        self.enabled()?;
        let reason = one_line(reason);
        let agenda: Vec<String> = agenda.iter().map(|item| one_line(item)).collect();
        if reason.is_empty()
            || reason.chars().count() > PHONE_PROPOSAL_REASON_MAX
            || agenda.len() > PHONE_PROPOSAL_AGENDA_MAX
            || agenda
                .iter()
                .any(|i| i.is_empty() || i.chars().count() > PHONE_PROPOSAL_ITEM_MAX)
        {
            return Err(Error::InvalidArguments);
        }
        let origin = self.proposal_origin(owner_id)?;
        if self.store.phone_destination_status(owner_id)?.masked.is_none() {
            return Err(Error::NotConfigured);
        }
        let now = self.clock.now_ms();
        let deliver_ms = self.config.proposals.delivery_ms(now);
        let proposal = self.store.phone_schedule_call_proposal(NewCallProposal {
            owner: owner_id,
            origin: &origin,
            reason: &reason,
            agenda: &agenda,
            now_ms: now,
            deliver_ms,
        })?;
        Ok(ProposalReceipt {
            id: proposal.id,
            deliver_ms,
            deferred: deliver_ms > now,
        })
    }

    /// The single owner every configured link names, for daemon-wide
    /// proactive loops. `None` when links name more than one owner.
    pub fn proposal_owner(&self) -> Option<String> {
        let mut owners = self.config.links.iter().map(|l| l.owner_id.as_str());
        let first = owners.next()?;
        owners.all(|o| o == first).then(|| first.to_owned())
    }

    /// The owner's private control conversation, re-authorized now.
    fn proposal_origin(&self, owner_id: &str) -> Result<PhoneOrigin, Error> {
        for link in self.config.links.iter().filter(|l| l.owner_id == owner_id) {
            let Ok(Some(binding)) = self.store.surface_owner_binding(link.surface_owner.account())
            else {
                continue;
            };
            let Some(conversation) = binding.direct_conversation() else {
                continue;
            };
            if self.authorize(&link.surface_owner, conversation).is_ok() {
                return Ok(PhoneOrigin {
                    owner: link.surface_owner.clone(),
                    conversation: conversation.clone(),
                });
            }
        }
        Err(Error::Unauthorized)
    }

    /// One delivery pass: expire unanswered proposals, then post each due
    /// question once through the chat adapter's durable outbox. Returns the
    /// number posted. Never dials.
    pub async fn deliver_call_proposals(&self, chat: &dyn ConversationAgent) -> Result<usize, Error> {
        let now = self.clock.now_ms();
        self.store.phone_expire_call_proposals(now)?;
        let mut posted = 0;
        if self.enabled().is_ok() {
            for proposal in self.store.phone_call_proposals_due(now)? {
                // Authority may have changed since the proposal was recorded.
                let fresh = self.authorize(&proposal.origin.owner, &proposal.origin.conversation);
                if fresh.map(|g| g.owner_id != proposal.owner).unwrap_or(true) {
                    continue;
                }
                let key = format!("call-proposal:{}", proposal.id);
                let text = question(&CallAgenda::from_proposal(&proposal));
                if chat.notify_owner(&proposal.origin, &key, &text).await.is_err() {
                    tracing::warn!(target: "phone.proposals", proposal_id = %proposal.id, "call proposal could not be posted; retrying");
                    continue;
                }
                if self.store.phone_mark_call_proposal_delivered(&proposal.id, now)? {
                    posted += 1;
                }
            }
        }
        self.refresh_consent_window()?;
        Ok(posted)
    }

    fn refresh_consent_window(&self) -> Result<(), Error> {
        let open = self
            .store
            .phone_call_proposal_window_open(self.clock.now_ms(), LATE_REPLY_GRACE_MS)?;
        self.set_consent_window(open);
        Ok(())
    }
    pub(crate) fn consent_window_open(&self) -> bool {
        self.consent_window.load(Ordering::SeqCst)
    }
    pub(crate) fn set_consent_window(&self, open: bool) {
        if self.consent_window.swap(open, Ordering::SeqCst) != open {
            if open {
                OPEN_WINDOWS.fetch_add(1, Ordering::SeqCst);
            } else {
                OPEN_WINDOWS.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// An owner's bare `yes`/`no` from an authenticated chat. Only the stored
    /// control conversation the question was posted in counts; a `yes` there
    /// within the window dials once via `call_owner`.
    pub(crate) async fn answer_proposal(
        &self,
        grant: &crate::Grant,
        accept: bool,
    ) -> Result<String, Error> {
        self.check(grant, false)?;
        let answer = self.store.phone_answer_call_proposal(
            &grant.owner_id,
            &grant.conversation.storage_key(),
            accept,
            self.clock.now_ms(),
            LATE_REPLY_GRACE_MS,
        )?;
        let reply = match answer {
            ProposalAnswer::Accepted(proposal) => {
                let request = proposal.call_request.ok_or(Error::Store(PhoneError::Conflict))?;
                // The existing `call me` path: same grant checks, limits and
                // idempotent reservation per request ID.
                let call = self
                    .tool(grant, "call_owner", serde_json::json!({ "request_id": request }))
                    .await?;
                format!(
                    "Calling you now. Phone call {}: {}.",
                    call["id"].as_str().unwrap_or("unknown"),
                    call["state"].as_str().unwrap_or("unknown")
                )
            }
            ProposalAnswer::AlreadyAccepted(proposal) => {
                // Same request ID: an existing reservation never dials again,
                // and one refused before it was created gets its first try.
                let request = proposal.call_request.ok_or(Error::Store(PhoneError::Conflict))?;
                let call = self
                    .tool(grant, "call_owner", serde_json::json!({ "request_id": request }))
                    .await?;
                format!(
                    "That call was already requested. Phone call {}: {}. Say `phone stop` to end it.",
                    call["id"].as_str().unwrap_or("unknown"),
                    call["state"].as_str().unwrap_or("unknown")
                )
            }
            ProposalAnswer::Declined | ProposalAnswer::AlreadyDeclined => DECLINED_REPLY.into(),
            ProposalAnswer::Expired => EXPIRED_REPLY.into(),
            ProposalAnswer::OtherConversation => OTHER_CONVERSATION_REPLY.into(),
            ProposalAnswer::NoProposal => NO_PROPOSAL_REPLY.into(),
        };
        self.refresh_consent_window()?;
        Ok(reply)
    }

    /// The agenda of the accepted proposal that requested this call.
    pub fn call_agenda(&self, call: &PhoneCall) -> Result<Option<CallAgenda>, Error> {
        Ok(self
            .store
            .phone_call_proposal_for_call(&call.id)?
            .filter(|p| p.owner == call.owner)
            .map(|p| CallAgenda::from_proposal(&p)))
    }
}
