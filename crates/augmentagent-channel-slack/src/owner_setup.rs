//! #1286 — setting up owner authority for an installed Slack app: bind the
//! owner, pick the private control channel, show and unbind. Used by
//! `augmentagent slack app owner …`; [`load_authorizer`] is what `serve`
//! (#1287) loads.
//!
//! Binding is live and strict. With the stored bot token it checks, in order:
//! the token still answers for the installed workspace (`auth.test`, which
//! also supplies the Enterprise Grid ID), then the user (`users.info`) is an
//! active, full member of that workspace — not a guest (`is_restricted`,
//! `is_ultra_restricted`), bot or app user, deactivated account, external
//! (`is_stranger`) or member of another team. Nothing is written unless every
//! check passes.
//!
//! The owner's DM with the app is resolved with `conversations.open`
//! (`im:write`). If that fails (for example an install made before the scope
//! was added), the binding is still made and the authorizer keeps the rule
//! "any DM with the app, from the owner, is a control conversation" until a
//! later bind records the DM.
//!
//! Identity is only ever the member ID. Names from `users.info` are shown to
//! the operator for confirmation, never stored or matched.

use augmentagent_store::owner::{ControlConversationKind, SurfaceOwnerBinding};
use augmentagent_store::{Store, StoreError, SurfaceConversationRef, SurfacePlatform};
use serde::Serialize;
use serde_json::Value;

use crate::app::{SlackAppConnector, SlackAppCredentials, SlackAppError, SlackAppStore};
use crate::owner::{SlackBotIdentity, SlackOwnerAuthority, SlackOwnerAuthorizer};
use crate::surface::{SlackWorkspace, SLACK_SURFACE_PLATFORM};
use crate::transport::web::WebApiError;

/// Why a user or channel was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerIneligibility {
    Guest,
    Bot,
    Deactivated,
    OtherWorkspace,
    External,
    Unverifiable,
    PublicChannel,
    ExternallyShared,
    Archived,
    AppNotMember,
    NotAChannel,
}

impl OwnerIneligibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guest => "guest",
            Self::Bot => "bot",
            Self::Deactivated => "deactivated",
            Self::OtherWorkspace => "other_workspace",
            Self::External => "external",
            Self::Unverifiable => "unverifiable",
            Self::PublicChannel => "public_channel",
            Self::ExternallyShared => "externally_shared",
            Self::Archived => "archived",
            Self::AppNotMember => "app_not_member",
            Self::NotAChannel => "not_a_channel",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Guest => "the account is a guest",
            Self::Bot => "the account is a bot or app user",
            Self::Deactivated => "the account is deactivated",
            Self::OtherWorkspace => "the account belongs to another workspace",
            Self::External => "the account is from a connected organization",
            Self::Unverifiable => "Slack did not report the account's workspace",
            Self::PublicChannel => "it is a public channel",
            Self::ExternallyShared => "it is shared with another organization",
            Self::Archived => "it is archived",
            Self::AppNotMember => "the app is not a member",
            Self::NotAChannel => "it is a DM or group DM, not a channel",
        }
    }
}

/// The owner's DM with the app after a bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum DirectConversation {
    /// `conversations.open` returned this DM ID; it is stored.
    Recorded(String),
    /// Could not be opened (the Slack error); any DM with the app from the
    /// owner is accepted until a later bind records it.
    Unresolved(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnerBindOutcome {
    pub team_id: String,
    pub enterprise_id: Option<String>,
    pub owner_user_id: String,
    /// For operator confirmation only; never stored.
    pub owner_name: Option<String>,
    pub owner_real_name: Option<String>,
    pub direct_conversation: DirectConversation,
    /// The previously bound owner, when this bind replaced someone else.
    pub replaced_owner: Option<String>,
    pub confirmed_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct OwnerStatus {
    pub team_id: String,
    pub binding: Option<SurfaceOwnerBinding>,
    /// From the install record; `None` when the app is not installed.
    pub bot: Option<SlackBotIdentity>,
    pub rejections: i64,
}

fn store_err(e: StoreError) -> SlackAppError {
    SlackAppError::Store(e.to_string())
}

fn slack_platform() -> SurfacePlatform {
    SurfacePlatform::new(SLACK_SURFACE_PLATFORM).expect("static platform")
}

/// The app's own identity, from the install record.
pub fn bot_identity(creds: &SlackAppCredentials) -> SlackBotIdentity {
    SlackBotIdentity {
        bot_user_id: Some(creds.bot_user_id.clone()).filter(|s| !s.is_empty()),
        bot_id: creds.bot_id.clone(),
        app_id: creds.app_id.clone(),
    }
}

/// The binding for `team_id`, whichever enterprise it was bound under.
pub fn find_binding(
    store: &Store,
    team_id: &str,
) -> Result<Option<SurfaceOwnerBinding>, SlackAppError> {
    Ok(store
        .surface_owner_bindings(&slack_platform())
        .map_err(store_err)?
        .into_iter()
        .find(|b| {
            SlackWorkspace::from_account(b.owner.account()).is_ok_and(|w| w.team_id() == team_id)
        }))
}

fn installed(store: &SlackAppStore, team_id: &str) -> Result<SlackAppCredentials, SlackAppError> {
    store
        .load(team_id)?
        .ok_or_else(|| SlackAppError::NotInstalled {
            team_id: team_id.to_string(),
        })
}

fn flag(raw: &Value, key: &str) -> bool {
    raw.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn user_eligibility(
    raw: &Value,
    user_id: &str,
    team_id: &str,
    enterprise_id: Option<&str>,
    bot_user_id: &str,
) -> Result<(), OwnerIneligibility> {
    if flag(raw, "deleted") {
        return Err(OwnerIneligibility::Deactivated);
    }
    if flag(raw, "is_bot") || flag(raw, "is_app_user") || user_id == bot_user_id {
        return Err(OwnerIneligibility::Bot);
    }
    if flag(raw, "is_restricted") || flag(raw, "is_ultra_restricted") {
        return Err(OwnerIneligibility::Guest);
    }
    if flag(raw, "is_stranger") {
        return Err(OwnerIneligibility::External);
    }
    let home = raw.get("team_id").and_then(Value::as_str);
    if home == Some(team_id) {
        return Ok(());
    }
    if let Some(enterprise) = enterprise_id {
        let eu = raw.get("enterprise_user");
        let same_enterprise = eu
            .and_then(|e| e.get("enterprise_id"))
            .and_then(Value::as_str)
            == Some(enterprise);
        let in_team = eu
            .and_then(|e| e.get("teams"))
            .and_then(Value::as_array)
            .is_some_and(|teams| teams.iter().any(|t| t.as_str() == Some(team_id)));
        if same_enterprise && in_team {
            return Ok(());
        }
        if same_enterprise {
            return Err(OwnerIneligibility::OtherWorkspace);
        }
    }
    match home {
        None => Err(OwnerIneligibility::Unverifiable),
        Some(_) => Err(OwnerIneligibility::OtherWorkspace),
    }
}

fn web_err(e: WebApiError) -> SlackAppError {
    match e {
        WebApiError::Slack { error, .. }
            if matches!(
                error.as_str(),
                "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked"
            ) =>
        {
            SlackAppError::InvalidToken {
                which: crate::app::TokenKind::Bot,
                slack_error: error,
            }
        }
        other => SlackAppError::SlackApi(crate::transport::token::redact(&other.to_string())),
    }
}

/// Verify `user_id` live and bind it as the owner of the installed
/// workspace (`team`, or the only installed one).
pub async fn bind_owner(
    store: &Store,
    creds: &SlackAppStore,
    connector: &dyn SlackAppConnector,
    team: Option<&str>,
    user_id: &str,
    now_ms: i64,
) -> Result<OwnerBindOutcome, SlackAppError> {
    let invalid_user = || SlackAppError::InvalidId {
        what: "member ID",
        value: user_id.chars().take(64).collect(),
    };
    // Shape check before any network call.
    SlackWorkspace::new("T0", None)
        .expect("static")
        .owner(user_id)
        .map_err(|_| invalid_user())?;
    let team_id = creds.resolve_team(team)?;
    let installed = installed(creds, &team_id)?;
    if user_id == installed.bot_user_id {
        return Err(SlackAppError::OwnerIneligible {
            user_id: user_id.to_string(),
            reason: OwnerIneligibility::Bot,
        });
    }
    let web = connector.web_api(&installed.bot_token)?;

    let who = web.auth_test().await.map_err(web_err)?;
    if who.team_id != team_id {
        return Err(SlackAppError::WrongWorkspace {
            expected: team_id,
            actual: who.team_id,
        });
    }
    let enterprise_id = who.enterprise_id.clone().filter(|e| !e.is_empty());
    let workspace = SlackWorkspace::new(&team_id, enterprise_id.as_deref()).map_err(|_| {
        SlackAppError::InvalidId {
            what: "team ID",
            value: team_id.clone(),
        }
    })?;
    let owner = workspace.owner(user_id).map_err(|_| invalid_user())?;

    let info = match web.user_info(user_id).await {
        Ok(info) => info,
        Err(WebApiError::Slack { error, .. })
            if matches!(error.as_str(), "user_not_found" | "user_not_visible") =>
        {
            return Err(SlackAppError::OwnerNotFound {
                user_id: user_id.to_string(),
            })
        }
        Err(e) => return Err(web_err(e)),
    };
    user_eligibility(
        &info.raw,
        user_id,
        &team_id,
        enterprise_id.as_deref(),
        &installed.bot_user_id,
    )
    .map_err(|reason| SlackAppError::OwnerIneligible {
        user_id: user_id.to_string(),
        reason,
    })?;

    let direct = match web.open_direct_conversation(user_id).await {
        Ok(id) => match workspace.conversation(&id, None) {
            Ok(conv) => Ok(conv),
            Err(_) => Err("conversations.open returned an invalid channel id".to_string()),
        },
        Err(WebApiError::Slack { error, .. }) => Err(error),
        Err(e) => Err(crate::transport::token::redact(&e.to_string())),
    };

    // Everything checked; write. A binding for this team under another
    // enterprise ID (the install moved) is replaced, not left behind.
    let previous = find_binding(store, &team_id)?;
    if let Some(prev) = &previous {
        if prev.owner.account() != owner.account() {
            store
                .unbind_surface_owner(prev.owner.account())
                .map_err(store_err)?;
        }
    }
    store
        .bind_surface_owner(&owner, now_ms)
        .map_err(store_err)?;
    let direct_conversation = match direct {
        Ok(conv) => {
            store
                .set_surface_control_conversation(&conv, ControlConversationKind::Direct, now_ms)
                .map_err(store_err)?;
            DirectConversation::Recorded(conv.conversation_id().to_string())
        }
        Err(reason) => DirectConversation::Unresolved(reason),
    };
    Ok(OwnerBindOutcome {
        team_id,
        enterprise_id,
        owner_user_id: user_id.to_string(),
        owner_name: info.name,
        owner_real_name: info.real_name,
        direct_conversation,
        replaced_owner: previous
            .map(|p| p.owner.sender_id().to_string())
            .filter(|p| p != user_id),
        confirmed_at_ms: now_ms,
    })
}

/// Local view: binding, bot identity and rejection count. No network.
pub fn owner_status(
    store: &Store,
    creds: &SlackAppStore,
    team: Option<&str>,
) -> Result<OwnerStatus, SlackAppError> {
    let team_id = creds.resolve_team(team)?;
    let binding = find_binding(store, &team_id)?;
    let bot = creds.load(&team_id)?.as_ref().map(bot_identity);
    let rejections = match &binding {
        Some(b) => store
            .surface_auth_rejection_count(b.owner.account())
            .map_err(store_err)?,
        None => 0,
    };
    Ok(OwnerStatus {
        team_id,
        binding,
        bot,
        rejections,
    })
}

/// Remove the owner binding (and its control conversations). Returns the
/// team and whether a binding existed.
pub fn unbind_owner(
    store: &Store,
    creds: &SlackAppStore,
    team: Option<&str>,
) -> Result<(String, bool), SlackAppError> {
    let team_id = creds.resolve_team(team)?;
    let removed = match find_binding(store, &team_id)? {
        Some(b) => store
            .unbind_surface_owner(b.owner.account())
            .map_err(store_err)?,
        None => false,
    };
    Ok((team_id, removed))
}

fn channel_eligibility(raw: &Value) -> Result<(), OwnerIneligibility> {
    if flag(raw, "is_im") || flag(raw, "is_mpim") {
        return Err(OwnerIneligibility::NotAChannel);
    }
    if !flag(raw, "is_private") {
        return Err(OwnerIneligibility::PublicChannel);
    }
    if flag(raw, "is_ext_shared") || flag(raw, "is_pending_ext_shared") {
        return Err(OwnerIneligibility::ExternallyShared);
    }
    if flag(raw, "is_archived") {
        return Err(OwnerIneligibility::Archived);
    }
    if raw.get("is_member").and_then(Value::as_bool) == Some(false) {
        return Err(OwnerIneligibility::AppNotMember);
    }
    Ok(())
}

/// Verify `channel_id` live (`conversations.info`) and make it the owner's
/// private control channel, replacing any previous one.
pub async fn set_control_channel(
    store: &Store,
    creds: &SlackAppStore,
    connector: &dyn SlackAppConnector,
    team: Option<&str>,
    channel_id: &str,
    now_ms: i64,
) -> Result<SurfaceConversationRef, SlackAppError> {
    let team_id = creds.resolve_team(team)?;
    let binding = find_binding(store, &team_id)?.ok_or_else(|| SlackAppError::OwnerNotBound {
        team_id: team_id.clone(),
    })?;
    let workspace = SlackWorkspace::from_account(binding.owner.account())
        .map_err(|e| SlackAppError::Store(e.to_string()))?;
    let conversation =
        workspace
            .conversation(channel_id, None)
            .map_err(|_| SlackAppError::InvalidId {
                what: "channel ID",
                value: channel_id.chars().take(64).collect(),
            })?;
    let installed = installed(creds, &team_id)?;
    let web = connector.web_api(&installed.bot_token)?;
    let info = match web.conversation_info(channel_id).await {
        Ok(info) => info,
        Err(WebApiError::Slack { error, .. }) if error == "channel_not_found" => {
            return Err(SlackAppError::ChannelNotFound {
                channel_id: channel_id.to_string(),
            })
        }
        Err(e) => return Err(web_err(e)),
    };
    let mut raw = info.raw.clone();
    if let Some(obj) = raw.as_object_mut() {
        // Typed flags win over a sparse raw record.
        for (k, v) in [
            ("is_im", info.is_im),
            ("is_mpim", info.is_mpim),
            ("is_private", info.is_private),
        ] {
            if v {
                obj.insert(k.into(), Value::Bool(true));
            }
        }
    }
    channel_eligibility(&raw).map_err(|reason| SlackAppError::ControlChannelIneligible {
        channel_id: channel_id.to_string(),
        reason,
    })?;
    store
        .set_surface_control_conversation(&conversation, ControlConversationKind::Channel, now_ms)
        .map_err(store_err)?;
    Ok(conversation)
}

/// Drop the private control channel; the DM stays. Returns the team and
/// whether one was set.
pub fn remove_control_channel(
    store: &Store,
    creds: &SlackAppStore,
    team: Option<&str>,
) -> Result<(String, bool), SlackAppError> {
    let team_id = creds.resolve_team(team)?;
    let removed = match find_binding(store, &team_id)?
        .as_ref()
        .and_then(|b| b.control_channel().cloned())
    {
        Some(conv) => store
            .remove_surface_control_conversation(&conv)
            .map_err(store_err)?,
        None => false,
    };
    Ok((team_id, removed))
}

/// Every stored Slack binding, with the installed bot identity attached
/// where the app is installed. What `serve` (#1287) authorizes with.
pub fn load_authorizer(
    store: &Store,
    creds: &SlackAppStore,
) -> Result<SlackOwnerAuthorizer, SlackAppError> {
    let mut authorities = Vec::new();
    for binding in store
        .surface_owner_bindings(&slack_platform())
        .map_err(store_err)?
    {
        let authority = SlackOwnerAuthority::from_binding(&binding)
            .map_err(|e| SlackAppError::Store(format!("stored Slack owner binding: {e}")))?;
        let bot = creds
            .load(authority.workspace().team_id())?
            .as_ref()
            .map(bot_identity);
        authorities.push(match bot {
            Some(bot) => authority.with_bot(bot),
            None => authority,
        });
    }
    Ok(SlackOwnerAuthorizer::new(authorities))
}
