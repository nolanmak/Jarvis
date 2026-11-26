//! Resolve a LinkedIn messaging-thread URL to the person behind it using only
//! the stored archive (#2503).
//!
//! A `/messaging/thread/<id>/` link carries nothing but an opaque conversation
//! id, and the live conversation lookup is not qualified yet, so the id is
//! matched against the participant snapshots archive sync already persists.
//! Nothing here opens a socket, drafts, sends or writes to the archive.
use crate::archive::Participant;
use crate::archive_store::{direct_peer, PeerLookup};
use crate::session_guard;
use augmentagent_store::Store;

/// The archive records a member urn and a display name, never a public profile
/// URL, so resolution reports the gap instead of inventing an `/in/` slug.
pub const PROFILE_URL_UNAVAILABLE: &str =
    "the archive records no public /in/ profile URL for this participant";

/// A rejected session is why a live read fails, never a reason the archive
/// would gain a mapping it never imported.
pub const RECONNECT_NOTE: &str =
    "reconnecting restores live reads but does not create the missing archive mapping";

/// Naming a person is opt-in, so the flag is part of every disabled answer.
pub const OPT_IN_FLAG: &str = "--enable-archive-resolution";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ThreadUrlError {
    #[error("not an https LinkedIn URL")]
    Host,
    #[error("not a LinkedIn messaging-thread URL; expected /messaging/thread/<id>")]
    Path,
    #[error("the messaging-thread URL carries no usable conversation id")]
    ConversationId,
}

/// The conversation id of a strictly parsed messaging-thread URL. Query and
/// fragment data are discarded.
pub fn parse_thread_url(input: &str) -> Result<String, ThreadUrlError> {
    let url = reqwest::Url::parse(input.trim()).map_err(|_| ThreadUrlError::Host)?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("www.linkedin.com" | "linkedin.com"))
    {
        return Err(ThreadUrlError::Host);
    }
    let mut segments: Vec<&str> = url.path_segments().ok_or(ThreadUrlError::Path)?.collect();
    if segments.last() == Some(&"") {
        segments.pop();
    }
    let ["messaging", "thread", id] = segments[..] else {
        return Err(ThreadUrlError::Path);
    };
    // The id is opaque, so only its shape is checked. Rejecting anything else
    // (percent escapes included) keeps it a plain identifier.
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | '~'))
    {
        return Err(ThreadUrlError::ConversationId);
    }
    Ok(id.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadUrlResolution {
    /// One archived non-owner participant of a direct thread.
    Resolved { conversation_id: String, participant: Participant },
    /// Archived, but naming one person from it would be a guess.
    Ambiguous { conversation_id: String, peers: usize, group: bool },
    /// The id never reached this account's archive: created after the last
    /// successful import, never captured, or no longer retained.
    MappingMissing { conversation_id: String },
    /// Not a messaging-thread URL at all.
    UnsupportedUrl(ThreadUrlError),
    /// The opt-in was not given, so nothing was read and no one is named.
    Disabled,
}

impl ThreadUrlResolution {
    pub fn state(&self) -> &'static str {
        match self {
            Self::Resolved { .. } => "resolved",
            Self::Ambiguous { .. } => "ambiguous",
            Self::MappingMissing { .. } => "archive_mapping_missing",
            Self::UnsupportedUrl(_) => "unsupported_url",
            Self::Disabled => "resolution_disabled",
        }
    }
    pub fn conversation_id(&self) -> Option<&str> {
        match self {
            Self::Resolved { conversation_id, .. }
            | Self::Ambiguous { conversation_id, .. }
            | Self::MappingMissing { conversation_id } => Some(conversation_id),
            Self::UnsupportedUrl(_) | Self::Disabled => None,
        }
    }
    /// One user-safe sentence. Carries no cursor, cookie or message body, and
    /// claims a person only when exactly one is recorded.
    pub fn detail(&self) -> String {
        match self {
            Self::Resolved { participant, .. } => format!(
                "archived as a direct thread with {} ({}); {PROFILE_URL_UNAVAILABLE}",
                participant.name, participant.urn
            ),
            Self::Ambiguous { peers, group, .. } => format!(
                "archived as {}, and its membership snapshot records {peers} other participants, \
                 so no single person follows from the link",
                if *group { "a group chat" } else { "a thread" }
            ),
            Self::MappingMissing { .. } => {
                "not in this account's stored LinkedIn archive, so the thread cannot be \
                 resolved offline; it postdates the last successful sync, was never \
                 captured, or is no longer retained"
                    .into()
            }
            Self::UnsupportedUrl(error) => error.to_string(),
            Self::Disabled => format!(
                "archive thread-URL resolution is off by default; pass {OPT_IN_FLAG} to look the \
                 conversation id up in the stored archive"
            ),
        }
    }
}

/// What the owner can do about a session LinkedIn is rejecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAdvice {
    pub since_ms: i64,
    pub reconnect_command: &'static str,
    /// Set when the archive has no mapping: a fresh login restores live reads
    /// but cannot import a conversation the archive never saw.
    pub note: Option<&'static str>,
}

/// The reconnect advice for `account`, or `None` while its session is healthy.
/// Carries no provider error text, so a stale session is never reported to the
/// owner as a redirect or request-compatibility problem.
pub fn session_advice(
    store: &Store,
    account: &str,
    resolution: &ThreadUrlResolution,
) -> anyhow::Result<Option<SessionAdvice>> {
    Ok(session_guard::dead_sessions(store)?
        .into_iter()
        .find(|dead| dead.account == account)
        .map(|dead| SessionAdvice {
            since_ms: dead.since_ms,
            reconnect_command: session_guard::LOGIN_COMMAND,
            note: matches!(resolution, ThreadUrlResolution::MappingMissing { .. })
                .then_some(RECONNECT_NOTE),
        }))
}

/// Resolve `url` against `account`'s archived participant snapshots.
///
/// Live conversation lookup stays out of scope until the session path is
/// qualified, so an unresolved thread is reported, never requested: a rejected
/// session cannot make this call fail. Naming a person is opt-in, so with
/// `enabled` unset neither the URL nor the archive is read.
pub fn resolve_thread_url(
    store: &Store,
    account: &str,
    url: &str,
    enabled: bool,
) -> anyhow::Result<ThreadUrlResolution> {
    if !enabled {
        return Ok(ThreadUrlResolution::Disabled);
    }
    let conversation_id = match parse_thread_url(url) {
        Ok(id) => id,
        Err(error) => return Ok(ThreadUrlResolution::UnsupportedUrl(error)),
    };
    Ok(match direct_peer(store, account, &conversation_id)? {
        PeerLookup::Peer(participant) => {
            ThreadUrlResolution::Resolved { conversation_id, participant }
        }
        PeerLookup::Ambiguous { peers, group } => {
            ThreadUrlResolution::Ambiguous { conversation_id, peers, group }
        }
        PeerLookup::Unknown => ThreadUrlResolution::MappingMissing { conversation_id },
    })
}

/// The owner-facing receipt for `url`, as JSON or plain lines. Lives here
/// rather than in the CLI so the rendering is covered by this crate's tests.
pub fn resolve_thread_url_report(
    store: &Store,
    account: &str,
    url: &str,
    json: bool,
    enabled: bool,
) -> anyhow::Result<String> {
    let resolution = resolve_thread_url(store, account, url, enabled)?;
    let session = session_advice(store, account, &resolution)?;
    if json {
        let account_id = enabled.then(|| format!("linkedin:{account}"));
        let mut receipt = serde_json::json!({"schema_version":1,"archive_only":true,"live_lookup_enabled":false,"network_requests":0,"account_id":account_id,"state":resolution.state(),"conversation_id":resolution.conversation_id(),"detail":resolution.detail()});
        if let ThreadUrlResolution::Resolved { participant, .. } = &resolution {
            receipt["participant"] = serde_json::json!({"name":participant.name,"member_urn":participant.urn,"profile_url":serde_json::Value::Null,"profile_url_unavailable":PROFILE_URL_UNAVAILABLE});
        }
        if let Some(session) = &session {
            receipt["session"] = serde_json::json!({"state":"rejected","since_ms":session.since_ms,"reconnect_command":session.reconnect_command,"note":session.note});
        }
        return Ok(format!("{}\n", serde_json::to_string_pretty(&receipt)?));
    }
    let mut output = format!("state: {}\n", resolution.state());
    if let Some(id) = resolution.conversation_id() {
        output.push_str(&format!("conversation_id: {id}\n"));
    }
    if let ThreadUrlResolution::Resolved { participant, .. } = &resolution {
        output.push_str(&format!(
            "participant: {} <linkedin:{}>\nprofile_url: unavailable ({PROFILE_URL_UNAVAILABLE})\n",
            participant.name, participant.urn
        ));
    }
    output.push_str(&format!("detail: {}\n", resolution.detail()));
    if let Some(session) = &session {
        output.push_str(&format!("session: rejected; run `{}`", session.reconnect_command));
        if let Some(note) = session.note {
            output.push_str(&format!("; {note}"));
        }
        output.push('\n');
    }
    output.push_str("archive_only=true; network_requests=0\n");
    Ok(output)
}
