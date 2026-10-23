//! #1286 — owner authority on Slack: who may start a turn, press an owner
//! card or run a command, and in which conversations.
//!
//! [`SlackOwnerAuthorizer::authorize`] is pure: it takes a parsed Socket Mode
//! [`EventEnvelope`] and the bound owner(s), and returns an [`AuthDecision`].
//! [`admit`] is the gate a runtime puts in front of the agent harness: owner
//! input goes to an [`OwnerInputSink`]; rejected input is written to the
//! store's rejection audit and never reaches the sink; ignored input is
//! dropped silently.
//!
//! ## Rules
//!
//! Authority is an exact match on `(workspace, user ID)` from the owner
//! binding (`augmentagent_store::owner`). Display names, usernames, profile
//! fields and emails are never read.
//!
//! 1. **Workspace.** The envelope's team must have a binding, and the
//!    enterprise ID must match it exactly (both absent, or equal). Enterprise
//!    Grid user IDs are org-wide, so the owner's ID from another team of the
//!    same enterprise does not carry authority. Otherwise `Reject`.
//! 2. **Events that are never a turn** are ignored before any identity check:
//!    edits and unfurls (`message_changed`), deletions, hidden messages,
//!    system subtypes, file and App Home events, anything unmodelled.
//! 3. **Echo and bots.** A message from this app's bot user, bot ID or app ID
//!    is `Ignore(OwnMessage)`; any other bot, workflow or integration post
//!    (`bot_id`, `bot_profile`, `app_id` or subtype `bot_message`) is
//!    `Ignore(BotOrIntegration)`, even when it claims the owner's user ID.
//! 4. **Acting user's team.** A message whose `user_team`, `source_team` or
//!    `team`, or an interaction whose `user.team_id`, names another team is
//!    `Reject(ForeignWorkspaceUser)`: Slack Connect users never gain
//!    authority, even with the owner's ID.
//! 5. **Messages** start a turn only in a control conversation: the bound
//!    DM, the bound private channel, or, until the DM ID is recorded, a DM
//!    with the app (`channel_type: im`). Chatter elsewhere is ignored. In a
//!    control conversation or a DM, anyone but the owner is rejected. An
//!    externally shared channel is rejected even for the owner.
//!    `app_mention` in a control conversation duplicates the `message` event
//!    and is ignored; elsewhere it is rejected.
//! 6. **Interactions and slash commands** are authorized by the acting user
//!    only, never by the channel they happened in.
//!
//! Every rejection carries the same short reply, [`REJECTION_REPLY`], which
//! reveals neither the owner nor the reason; no reply is offered for an
//! unbound workspace, an enterprise mismatch or a payload without an actor.

use augmentagent_store::owner::{
    ControlConversationKind, NewAuthRejection, SurfaceOwnerBinding, AUDIT_FIELD_MAX,
};
use augmentagent_store::{
    Store, StoreResult, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef,
    SurfacePlatform, SurfaceRefError,
};
use serde_json::Value;

use crate::surface::{SlackWorkspace, SLACK_SURFACE_PLATFORM};
use crate::transport::event::{EnvelopeKind, EventEnvelope, MessageEvent, SlackEvent};

/// The only text a rejected user ever sees.
pub const REJECTION_REPLY: &str = "Sorry, I only take requests from my owner here.";

/// Subtypes that are ordinary human messages. Everything else is a system,
/// bot or hidden message.
const HUMAN_SUBTYPES: [&str; 2] = ["file_share", "thread_broadcast"];

/// The app's own identity in one workspace, from its install record
/// (`auth.test`). Used only to tell the agent's echoes from other bots; both
/// are ignored either way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackBotIdentity {
    pub bot_user_id: Option<String>,
    pub bot_id: Option<String>,
    pub app_id: Option<String>,
}

/// The bound owner of one workspace and its control conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackOwnerAuthority {
    workspace: SlackWorkspace,
    owner: SurfaceOwnerRef,
    direct_conversation: Option<String>,
    control_channel: Option<String>,
    bot: SlackBotIdentity,
}

impl SlackOwnerAuthority {
    pub fn new(workspace: SlackWorkspace, owner_user_id: &str) -> Result<Self, SurfaceRefError> {
        let owner = workspace.owner(owner_user_id)?;
        Ok(Self {
            workspace,
            owner,
            direct_conversation: None,
            control_channel: None,
            bot: SlackBotIdentity::default(),
        })
    }

    pub fn with_direct_conversation(mut self, channel_id: &str) -> Result<Self, SurfaceRefError> {
        self.workspace.conversation(channel_id, None)?;
        self.direct_conversation = Some(channel_id.to_string());
        Ok(self)
    }

    pub fn with_control_channel(mut self, channel_id: &str) -> Result<Self, SurfaceRefError> {
        self.workspace.conversation(channel_id, None)?;
        self.control_channel = Some(channel_id.to_string());
        Ok(self)
    }

    pub fn with_bot(mut self, bot: SlackBotIdentity) -> Self {
        self.bot = bot;
        self
    }

    /// From a stored binding. Refuses bindings of other platforms.
    pub fn from_binding(binding: &SurfaceOwnerBinding) -> Result<Self, SurfaceRefError> {
        let workspace = SlackWorkspace::from_account(binding.owner.account())?;
        let mut authority = Self::new(workspace, binding.owner.sender_id())?;
        for control in &binding.control {
            let id = control.conversation.conversation_id();
            authority = match control.kind {
                ControlConversationKind::Direct => authority.with_direct_conversation(id)?,
                ControlConversationKind::Channel => authority.with_control_channel(id)?,
            };
        }
        Ok(authority)
    }

    pub fn workspace(&self) -> &SlackWorkspace {
        &self.workspace
    }

    pub fn owner(&self) -> &SurfaceOwnerRef {
        &self.owner
    }

    fn is_owner(&self, user_id: &str) -> bool {
        self.owner.sender_id() == user_id
    }

    fn is_control(&self, channel_id: &str) -> bool {
        self.direct_conversation.as_deref() == Some(channel_id)
            || self.control_channel.as_deref() == Some(channel_id)
    }
}

/// Every bound workspace, looked up by the envelope's team.
#[derive(Debug, Clone, Default)]
pub struct SlackOwnerAuthorizer {
    authorities: Vec<SlackOwnerAuthority>,
}

/// What entered the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerInputSource {
    Message,
    ThreadReply,
    Interaction,
    SlashCommand,
}

/// Input the bound owner is allowed to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerInput {
    pub owner: SurfaceOwnerRef,
    /// Where it happened. `None` for interactions without a channel (for
    /// example a modal submission).
    pub conversation: Option<SurfaceConversationRef>,
    pub source: OwnerInputSource,
}

/// Why an event produces no turn and no reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Posted by this app: never answer our own output.
    OwnMessage,
    /// Another bot, a workflow or an integration.
    BotOrIntegration,
    /// `message_changed`, including link-unfurl updates.
    EditedMessage,
    DeletedMessage,
    HiddenMessage,
    /// Joins, topic changes, pins and other non-human subtypes.
    SystemSubtype,
    /// A message somewhere that is not a control conversation.
    NotControlConversation,
    /// `app_mention` in a control conversation, already seen as `message`.
    DuplicateMention,
    /// No usable channel, user or timestamp.
    Malformed,
    /// App Home, file events and anything else that is never a turn.
    NotATurn,
}

impl IgnoreReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnMessage => "own_message",
            Self::BotOrIntegration => "bot_or_integration",
            Self::EditedMessage => "edited_message",
            Self::DeletedMessage => "deleted_message",
            Self::HiddenMessage => "hidden_message",
            Self::SystemSubtype => "system_subtype",
            Self::NotControlConversation => "not_control_conversation",
            Self::DuplicateMention => "duplicate_mention",
            Self::Malformed => "malformed",
            Self::NotATurn => "not_a_turn",
        }
    }
}

/// Why input was refused. Every rejection is audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// No owner is bound for the envelope's team.
    UnboundWorkspace,
    /// The team is bound, but under a different (or no) enterprise.
    EnterpriseMismatch,
    /// The acting user belongs to another team (Slack Connect).
    ForeignWorkspaceUser,
    /// A control channel that is shared with another organization.
    ExternallySharedChannel,
    /// Someone other than the bound owner.
    NotOwner,
    /// The owner, addressing the app outside a control conversation.
    NotControlConversation,
    /// An interaction or command with no acting user.
    MissingActor,
}

impl RejectReason {
    /// Stable audit code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnboundWorkspace => "unbound_workspace",
            Self::EnterpriseMismatch => "enterprise_mismatch",
            Self::ForeignWorkspaceUser => "foreign_workspace_user",
            Self::ExternallySharedChannel => "externally_shared_channel",
            Self::NotOwner => "not_owner",
            Self::NotControlConversation => "not_control_conversation",
            Self::MissingActor => "missing_actor",
        }
    }
}

/// A refusal plus the identifiers needed to audit it. Never carries message
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub reason: RejectReason,
    /// The account the envelope claimed; for an unbound or unparseable team
    /// this is still where the audit row is filed.
    pub account: SurfaceAccountRef,
    pub conversation_id: Option<String>,
    pub actor_id: Option<String>,
    pub event_kind: String,
    pub event_id: Option<String>,
}

impl Rejection {
    /// The reply to post back, if any. Identical for every reason, so it
    /// reveals nothing about the owner or the rule that fired.
    pub fn reply_text(&self) -> Option<&'static str> {
        match self.reason {
            RejectReason::UnboundWorkspace
            | RejectReason::EnterpriseMismatch
            | RejectReason::MissingActor => None,
            RejectReason::ForeignWorkspaceUser
            | RejectReason::ExternallySharedChannel
            | RejectReason::NotOwner
            | RejectReason::NotControlConversation => Some(REJECTION_REPLY),
        }
    }

    /// The audit row. Identifiers come from the (possibly hostile) payload,
    /// so each is clipped to the store's bound rather than failing the write.
    pub fn audit_record(&self, now_ms: i64) -> NewAuthRejection {
        let clip_opt = |v: &Option<String>| v.as_deref().map(clip);
        NewAuthRejection {
            account: self.account.clone(),
            conversation_id: clip_opt(&self.conversation_id),
            actor_id: clip_opt(&self.actor_id),
            event_kind: clip(&self.event_kind),
            event_id: clip_opt(&self.event_id),
            reason: self.reason.as_str().to_string(),
            occurred_at_ms: now_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    Owner(OwnerInput),
    Ignore(IgnoreReason),
    Reject(Rejection),
}

/// At most [`AUDIT_FIELD_MAX`] bytes, cut on a character boundary.
fn clip(value: &str) -> String {
    if value.len() <= AUDIT_FIELD_MAX {
        return value.to_string();
    }
    let mut end = AUDIT_FIELD_MAX;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `(team, enterprise, acting user's team)` as the envelope states them.
fn envelope_scope(envelope: &EventEnvelope) -> (Option<&str>, Option<&str>, Option<&str>) {
    let p = &envelope.payload;
    match envelope.kind {
        EnvelopeKind::EventsApi => (
            envelope
                .events_api
                .as_ref()
                .and_then(|m| m.team_id.as_deref())
                .filter(|s| !s.is_empty()),
            str_field(p, "enterprise_id"),
            None,
        ),
        EnvelopeKind::Interactive => (
            p.get("team").and_then(|t| str_field(t, "id")),
            p.get("enterprise")
                .and_then(|e| str_field(e, "id"))
                .or_else(|| str_field(p, "enterprise_id")),
            p.get("user").and_then(|u| str_field(u, "team_id")),
        ),
        EnvelopeKind::SlashCommands => (
            str_field(p, "team_id"),
            str_field(p, "enterprise_id"),
            str_field(p, "user_team"),
        ),
        EnvelopeKind::Other(_) => (None, None, None),
    }
}

fn unknown_account() -> SurfaceAccountRef {
    SurfaceAccountRef::new(
        SurfacePlatform::new(SLACK_SURFACE_PLATFORM).expect("static platform"),
        "unknown",
    )
    .expect("static account")
}

/// Actor and conversation of an event, for the audit row.
fn audit_ids(event: &SlackEvent) -> (Option<String>, Option<String>) {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    match event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => {
            (m.user.clone(), non_empty(&m.channel))
        }
        SlackEvent::Interaction(i) => (i.user_id.clone(), i.channel_id.clone()),
        SlackEvent::SlashCommand(c) => (non_empty(&c.user_id), c.channel_id.clone()),
        SlackEvent::MessageEdited(e) => (e.user.clone(), non_empty(&e.channel)),
        SlackEvent::MessageDeleted(d) => (None, non_empty(&d.channel)),
        SlackEvent::File(f) => (f.user_id.clone(), f.channel_id.clone()),
        SlackEvent::AppHome(h) => (non_empty(&h.user_id), h.channel.clone()),
        SlackEvent::Unknown { .. } => (None, None),
    }
}

fn event_kind(event: &SlackEvent) -> &str {
    match event {
        SlackEvent::Unknown { .. } => "unknown",
        other => other.kind_name(),
    }
}

impl SlackOwnerAuthorizer {
    pub fn new(authorities: Vec<SlackOwnerAuthority>) -> Self {
        Self { authorities }
    }

    /// Every Slack binding in the store. A stored binding that no longer maps
    /// to a Slack workspace is an error, not a silently skipped owner.
    pub fn load(store: &Store) -> StoreResult<Self> {
        let platform = SurfacePlatform::new(SLACK_SURFACE_PLATFORM).expect("static platform");
        let authorities = store
            .surface_owner_bindings(&platform)?
            .iter()
            .map(|binding| {
                SlackOwnerAuthority::from_binding(binding).map_err(|e| {
                    augmentagent_store::StoreError::InvalidInput(format!(
                        "stored Slack owner binding: {e}"
                    ))
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        Ok(Self { authorities })
    }

    /// Attach the app's bot identity for one bound workspace.
    pub fn with_bot(mut self, workspace: &SlackWorkspace, bot: SlackBotIdentity) -> Self {
        for authority in &mut self.authorities {
            if &authority.workspace == workspace {
                authority.bot = bot.clone();
            }
        }
        self
    }

    pub fn authorities(&self) -> &[SlackOwnerAuthority] {
        &self.authorities
    }

    pub fn authorize(&self, envelope: &EventEnvelope) -> AuthDecision {
        let event = &envelope.event;
        let (team, enterprise, actor_team) = envelope_scope(envelope);

        let reject = |reason: RejectReason, account: SurfaceAccountRef| {
            let (actor_id, conversation_id) = audit_ids(event);
            AuthDecision::Reject(Rejection {
                reason,
                account,
                conversation_id,
                actor_id,
                event_kind: event_kind(event).to_string(),
                event_id: Some(envelope.stable_id()),
            })
        };

        // 1. Workspace and enterprise.
        let claimed_account = team
            .and_then(|t| SlackWorkspace::new(t, enterprise).ok())
            .map(|w| w.account())
            .unwrap_or_else(unknown_account);
        let Some(authority) =
            team.and_then(|t| self.authorities.iter().find(|a| a.workspace.team_id() == t))
        else {
            return reject(RejectReason::UnboundWorkspace, claimed_account);
        };
        if authority.workspace.enterprise_id() != enterprise {
            return reject(RejectReason::EnterpriseMismatch, claimed_account);
        }
        let account = authority.workspace.account();
        if actor_team.is_some_and(|t| t != authority.workspace.team_id()) {
            return reject(RejectReason::ForeignWorkspaceUser, account);
        }

        match event {
            SlackEvent::Message(m) => self.message(envelope, authority, m, false, &reject),
            SlackEvent::ThreadReply(m) => self.message(envelope, authority, m, false, &reject),
            SlackEvent::AppMention(m) => self.message(envelope, authority, m, true, &reject),
            SlackEvent::MessageEdited(_) => AuthDecision::Ignore(IgnoreReason::EditedMessage),
            SlackEvent::MessageDeleted(_) => AuthDecision::Ignore(IgnoreReason::DeletedMessage),
            SlackEvent::Interaction(i) => {
                let Some(user) = i.user_id.as_deref().filter(|u| !u.is_empty()) else {
                    return reject(RejectReason::MissingActor, account);
                };
                if !authority.is_owner(user) {
                    return reject(RejectReason::NotOwner, account);
                }
                let thread = envelope
                    .payload
                    .get("message")
                    .and_then(|m| str_field(m, "thread_ts"))
                    .or_else(|| {
                        envelope
                            .payload
                            .get("container")
                            .and_then(|c| str_field(c, "thread_ts"))
                    });
                let conversation = match i.channel_id.as_deref() {
                    Some(channel) => match authority
                        .workspace
                        .conversation(channel, thread)
                        .or_else(|_| authority.workspace.conversation(channel, None))
                    {
                        Ok(c) => Some(c),
                        Err(_) => return AuthDecision::Ignore(IgnoreReason::Malformed),
                    },
                    None => None,
                };
                AuthDecision::Owner(OwnerInput {
                    owner: authority.owner.clone(),
                    conversation,
                    source: OwnerInputSource::Interaction,
                })
            }
            SlackEvent::SlashCommand(c) => {
                if c.user_id.is_empty() {
                    return reject(RejectReason::MissingActor, account);
                }
                if !authority.is_owner(&c.user_id) {
                    return reject(RejectReason::NotOwner, account);
                }
                let conversation = match c.channel_id.as_deref() {
                    Some(channel) => match authority.workspace.conversation(channel, None) {
                        Ok(conv) => Some(conv),
                        Err(_) => return AuthDecision::Ignore(IgnoreReason::Malformed),
                    },
                    None => None,
                };
                AuthDecision::Owner(OwnerInput {
                    owner: authority.owner.clone(),
                    conversation,
                    source: OwnerInputSource::SlashCommand,
                })
            }
            SlackEvent::File(_) | SlackEvent::AppHome(_) | SlackEvent::Unknown { .. } => {
                AuthDecision::Ignore(IgnoreReason::NotATurn)
            }
        }
    }

    fn message(
        &self,
        envelope: &EventEnvelope,
        authority: &SlackOwnerAuthority,
        m: &MessageEvent,
        mention: bool,
        reject: &impl Fn(RejectReason, SurfaceAccountRef) -> AuthDecision,
    ) -> AuthDecision {
        let account = authority.workspace.account();
        let raw = &m.raw;
        let bot = &authority.bot;

        // 2. Never a turn.
        if raw.get("hidden").and_then(Value::as_bool) == Some(true) {
            return AuthDecision::Ignore(IgnoreReason::HiddenMessage);
        }
        // 3. Our own output first, then any other automated poster.
        let app_id = str_field(raw, "app_id");
        let own = (m.bot_id.is_some() && m.bot_id == bot.bot_id)
            || (m.user.is_some() && m.user == bot.bot_user_id)
            || (app_id.is_some() && app_id == bot.app_id.as_deref());
        if own {
            return AuthDecision::Ignore(IgnoreReason::OwnMessage);
        }
        if m.bot_id.is_some()
            || app_id.is_some()
            || raw.get("bot_profile").is_some_and(|v| !v.is_null())
            || m.subtype.as_deref() == Some("bot_message")
        {
            return AuthDecision::Ignore(IgnoreReason::BotOrIntegration);
        }
        if let Some(subtype) = m.subtype.as_deref() {
            if !HUMAN_SUBTYPES.contains(&subtype) {
                return AuthDecision::Ignore(IgnoreReason::SystemSubtype);
            }
        }
        let Some(user) = m.user.as_deref().filter(|u| !u.is_empty()) else {
            return AuthDecision::Ignore(IgnoreReason::Malformed);
        };
        if m.channel.is_empty() {
            return AuthDecision::Ignore(IgnoreReason::Malformed);
        }

        // 4. The acting user's own team.
        let team = authority.workspace.team_id();
        if ["user_team", "source_team", "team"]
            .iter()
            .filter_map(|k| str_field(raw, k))
            .any(|t| t != team)
        {
            return reject(RejectReason::ForeignWorkspaceUser, account);
        }

        // 5. Where it was said.
        let is_im = m.channel_type.as_deref() == Some("im");
        let bound_control = authority.is_control(&m.channel);
        let unbound_dm = is_im && authority.direct_conversation.is_none();
        let control = bound_control || unbound_dm;
        if mention && control {
            return AuthDecision::Ignore(IgnoreReason::DuplicateMention);
        }
        let addressed = control || is_im || mention;
        if !addressed {
            return AuthDecision::Ignore(IgnoreReason::NotControlConversation);
        }
        if !authority.is_owner(user) {
            return reject(RejectReason::NotOwner, account);
        }
        if !control {
            return reject(RejectReason::NotControlConversation, account);
        }
        if !is_im
            && envelope
                .payload
                .get("is_ext_shared_channel")
                .and_then(Value::as_bool)
                == Some(true)
        {
            return reject(RejectReason::ExternallySharedChannel, account);
        }

        let thread = m.thread_ts.as_deref().filter(|t| *t != m.ts);
        let Ok(conversation) = authority.workspace.conversation(&m.channel, thread) else {
            return AuthDecision::Ignore(IgnoreReason::Malformed);
        };
        AuthDecision::Owner(OwnerInput {
            owner: authority.owner.clone(),
            conversation: Some(conversation),
            source: if thread.is_some() {
                OwnerInputSource::ThreadReply
            } else {
                OwnerInputSource::Message
            },
        })
    }
}

/// The agent harness behind the gate: the only place owner input goes.
pub trait OwnerInputSink {
    fn owner_input(&mut self, input: OwnerInput, envelope: &EventEnvelope);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitOutcome {
    /// Handed to the sink.
    Dispatched,
    Ignored(IgnoreReason),
    /// Audited; `reply`, if any, is what the caller may post back (ephemeral
    /// or `response_url`), and it must not be routed through the harness.
    Rejected {
        reason: RejectReason,
        reply: Option<&'static str>,
        audit_id: i64,
    },
}

/// Authorize `envelope` and route it: owner input to `sink`, rejections to
/// the audit log (before returning, so a crash cannot lose the record),
/// everything else nowhere.
pub fn admit(
    store: &Store,
    authorizer: &SlackOwnerAuthorizer,
    envelope: &EventEnvelope,
    now_ms: i64,
    sink: &mut impl OwnerInputSink,
) -> StoreResult<AdmitOutcome> {
    match authorizer.authorize(envelope) {
        AuthDecision::Owner(input) => {
            sink.owner_input(input, envelope);
            Ok(AdmitOutcome::Dispatched)
        }
        AuthDecision::Ignore(reason) => Ok(AdmitOutcome::Ignored(reason)),
        AuthDecision::Reject(rejection) => {
            let audit_id = store.record_surface_auth_rejection(&rejection.audit_record(now_ms))?;
            Ok(AdmitOutcome::Rejected {
                reason: rejection.reason,
                reply: rejection.reply_text(),
                audit_id,
            })
        }
    }
}
