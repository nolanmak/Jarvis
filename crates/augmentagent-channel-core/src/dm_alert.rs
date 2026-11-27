//! #2422 — shared, default-off policy for "a new DM arrived" owner alerts. An
//! inbound 1:1 DM qualifies when it is the conversation's first stored inbound
//! contact, or when its sender resolves — through the wiki identity index only
//! — to a person on the owner's priority list. Every platform stays `off`
//! until `AUGMENTAGENT_ALERT_DM_POLICY` names it, and a malformed or repeated
//! entry leaves that platform off rather than guessing. Nothing here reaches
//! the DM's sender: a qualifying event only creates the High owner alert the
//! existing schedule and the opted-in owner-text destination already deliver.

use std::collections::{BTreeMap, BTreeSet};

use augmentagent_store::{
    owner_alert_gate::person_is_priority, owner_alerts::NewDmAlert, surface::SurfacePlatform,
    Store, StoreResult,
};
use augmentagent_wiki::IdentityIndex;

pub const POLICY_ENV: &str = "AUGMENTAGENT_ALERT_DM_POLICY";

/// What the owner opted a platform into: nothing, only resolved owner-priority
/// contacts, only a conversation's first inbound contact, or both classes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DmAlertMode {
    #[default]
    Off,
    Priority,
    FirstContact,
    All,
}

/// The only accepted spellings; anything else leaves the platform off.
const MODES: [(&str, DmAlertMode); 4] = [("off", DmAlertMode::Off),
    ("priority", DmAlertMode::Priority), ("first-contact", DmAlertMode::FirstContact),
    ("all", DmAlertMode::All)];

/// Per-platform opt-in, parsed from `AUGMENTAGENT_ALERT_DM_POLICY`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DmAlertPolicy {
    modes: BTreeMap<String, DmAlertMode>,
}

impl DmAlertPolicy {
    /// `<platform>=<off|priority|first-contact|all>,…`, matched verbatim — no
    /// trimming, no case folding — so a typo cannot silently enable
    /// notifications. A platform named twice, or named with an unknown mode,
    /// stays off even when another entry would have enabled it; a rejected
    /// entry never affects a different platform.
    pub fn parse(raw: &str) -> Self {
        let mut modes = BTreeMap::new();
        let mut rejected = BTreeSet::new();
        for entry in raw.split(',').filter(|entry| !entry.is_empty()) {
            // A bare `imessage` names its platform but no mode: reject that
            // platform rather than keep an earlier entry's mode for it.
            let (platform, mode) = entry.split_once('=').unwrap_or((entry, ""));
            let Ok(platform) = SurfacePlatform::new(platform) else { continue };
            let platform = platform.as_str().to_string();
            let first = !rejected.contains(&platform) && !modes.contains_key(&platform);
            match MODES.iter().find(|(name, _)| *name == mode) {
                Some((_, mode)) if first => { modes.insert(platform, *mode); }
                _ => { modes.remove(&platform); rejected.insert(platform); }
            }
        }
        Self { modes }
    }

    pub fn from_env() -> Self {
        Self::parse(&std::env::var(POLICY_ENV).unwrap_or_default())
    }

    pub fn mode(&self, platform: &str) -> DmAlertMode {
        self.modes.get(platform).copied().unwrap_or_default()
    }
}

/// Authoritative transport metadata for one inbound event, taken from the
/// channel rather than inferred from message text. `conversation_id` keys the
/// dedupe window; `handle` is looked up verbatim in the wiki index;
/// `direct_message` is `false` for every group and channel; `prior_inbound` is
/// the channel's own answer to "has this conversation stored an inbound message
/// before?", so opting a long-running conversation in is not a first contact.
#[derive(Debug, Clone, Copy)]
pub struct DmEvent<'a> {
    pub platform: &'a str,
    pub conversation_id: &'a str,
    pub handle: &'a str,
    /// Concise sender label for the alert, when the channel knows a name.
    pub display_name: &'a str,
    pub source_url: &'a str,
    pub inbound: bool,
    pub direct_message: bool,
    pub from_self: bool,
    pub from_bot: bool,
    pub prior_inbound: bool,
}

impl<'a> DmEvent<'a> {
    /// An inbound 1:1 DM from a human: the only shape that can ever qualify.
    pub fn inbound_dm(platform: &'a str, conversation_id: &'a str, handle: &'a str, source_url: &'a str) -> Self {
        Self { platform, conversation_id, handle, display_name: handle, source_url, inbound: true,
            direct_message: true, from_self: false, from_bot: false, prior_inbound: false }
    }
}

/// Evaluate one inbound event and, when it qualifies, create the High owner
/// alert. Call this *before* persisting the message, so the conversation's own
/// stored history does not yet contain it. `index` is the only identity source:
/// an unresolved handle is never a priority contact and there is no fuzzy
/// matching, so an empty index permits only first-contact alerts.
pub fn evaluate(
    store: &Store, policy: &DmAlertPolicy, event: &DmEvent<'_>, index: &IdentityIndex, now_ms: i64,
) -> StoreResult<Option<String>> {
    // Only an inbound 1:1 message from a human can ever qualify, so a group,
    // channel, bot, self or outbound event never reaches the store.
    let human_dm = event.inbound && event.direct_message && !event.from_self && !event.from_bot;
    if !human_dm || event.handle.trim().is_empty() || event.conversation_id.trim().is_empty() {
        return Ok(None);
    }
    let mode = policy.mode(event.platform);
    let allow_first_contact = matches!(mode, DmAlertMode::FirstContact | DmAlertMode::All);
    let resolvable = matches!(mode, DmAlertMode::Priority | DmAlertMode::All);
    let person = index.lookup(event.platform, event.handle).filter(|_| resolvable);
    let allow_priority = match person {
        Some(p) => person_is_priority(&store.owner_alert_priorities()?, &p.slug, &p.identities.email),
        None => false,
    };
    if !allow_first_contact && !allow_priority {
        return Ok(None);
    }
    let timezone = std::env::var("AUGMENTAGENT_ALERT_TIMEZONE").unwrap_or_else(|_| "UTC".into());
    store.create_dm_owner_alert(
        &NewDmAlert {
            platform: event.platform, conversation_id: event.conversation_id,
            sender: event.display_name, source_url: event.source_url, timezone: &timezone,
            allow_first_contact, allow_priority, prior_inbound: event.prior_inbound,
        },
        now_ms,
    )
}

/// Where a DM alert points the owner: the dashboard's message history, read
/// from the dashboard's own `AUGMENTAGENT_PUBLIC_URL` / `DASHBOARD_PORT`. A
/// non-HTTP(S) origin is ignored, not allowed to fail every alert.
pub fn dashboard_history_url() -> String {
    let public = std::env::var("AUGMENTAGENT_PUBLIC_URL").unwrap_or_default();
    let public = public.trim().trim_end_matches('/');
    if public.starts_with("https://") || public.starts_with("http://") {
        return format!("{public}/history");
    }
    format!("http://localhost:{}/history",
        std::env::var("DASHBOARD_PORT").unwrap_or_else(|_| "3000".into()))
}

/// Channel-adapter entry point (#2424): evaluate `event` under the policy in
/// the environment before the channel persists, triages or drafts the
/// message. A disabled platform touches nothing. `history_thread` is the
/// channel's stored thread ID for this conversation, so a chat with earlier
/// stored messages is not a first contact the day the policy is enabled.
/// Without a wiki `index` no sender resolves, so only first contacts qualify.
/// A failure is logged, never returned: a missed notification must not drop
/// or delay the message itself.
pub fn evaluate_inbound(
    store: &Store, mut event: DmEvent<'_>, history_thread: Option<&str>, index: Option<&IdentityIndex>,
) -> Option<String> {
    let policy = DmAlertPolicy::from_env();
    if policy.mode(event.platform) == DmAlertMode::Off {
        return None;
    }
    if event.display_name.trim().is_empty() {
        event.display_name = event.handle;
    }
    if let Some(thread) = history_thread {
        event.prior_inbound |= thread_has_history(store, event.platform, thread);
    }
    let empty = IdentityIndex::default();
    let now_ms = chrono::Utc::now().timestamp_millis();
    match evaluate(store, &policy, &event, index.unwrap_or(&empty), now_ms) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(platform = event.platform, error = %e, "new-DM alert evaluation failed");
            None
        }
    }
}

/// Whether the channel already stored a message in `thread_id`. A failed
/// lookup counts as history: it can cost a first-contact alert, never invent one.
fn thread_has_history(store: &Store, platform: &str, thread_id: &str) -> bool {
    let sql = "SELECT EXISTS(SELECT 1 FROM emails WHERE platform=?1 AND threadId=?2)";
    store
        .with_conn(|c| c.query_row(sql, [platform, thread_id], |r| r.get(0)))
        .inspect_err(|e| tracing::warn!(platform, error = %e, "DM history lookup failed"))
        .unwrap_or(true)
}
