//! Channel-neutral direct-message alert policy (#1643).
//!
//! Adapters call [`evaluate`] once their own admission guards have accepted
//! an inbound 1:1 message, and before they persist, batch, triage, draft or
//! ingest it — so an urgent DM still reaches the owner when that downstream
//! work fails. The evaluator may persist an owner alert; it never replies to
//! or otherwise contacts the sender. Opt-in like the email gate: without
//! [`ENABLED`] every call is a no-op, so each channel's existing allowlist
//! and control-message behaviour is unchanged.
use augmentagent_store::{
    owner_alert_gate::{bare_sender, selector_matches},
    owner_alerts::{NewOwnerAlert, Urgency},
    Store,
};
use sha2::{Digest, Sha256};
use tracing::warn;

pub const ENABLED: &str = "AUGMENTAGENT_ALERT_DM_GATES";
pub fn enabled() -> bool {
    std::env::var(ENABLED).is_ok_and(|v| matches!(v.as_str(), "1" | "true"))
}

/// One alert per conversation per rolling hour, measured against the durable
/// `owner_alerts` rows so a restart cannot reopen the window.
pub const WINDOW_MS: i64 = 3_600_000;
/// Usefulness cutoff for an alert with no deadline, as for email arrivals.
const USEFUL_MS: i64 = 86_400_000;

/// An inbound 1:1 message a channel has already admitted. Groups, channels,
/// bots, self-authored and outbound traffic never reach this type — the
/// channel-local guards remain the authoritative policy boundary.
pub struct InboundDm<'a> {
    pub platform: &'a str,
    /// The channel's own conversation identifier: the durable dedupe subject.
    pub conversation: &'a str,
    /// Selector-shaped sender handles (address, bare JID, E.164 phone), and
    /// any wiki person slugs the channel already resolved for this sender.
    pub handles: &'a [String],
    pub person_slugs: &'a [String],
    pub sender_label: &'a str,
    /// Channel reference the owner can open from the alert.
    pub source_url: &'a str,
}

/// Same knob as the email alert path.
fn timezone() -> String {
    std::env::var("AUGMENTAGENT_ALERT_TIMEZONE").unwrap_or_else(|_| "UTC".into())
}

/// Evaluate one admitted DM. Returns the new alert id, or `None` when the
/// gate is off, no owner priority resolves, the conversation already alerted
/// within the rolling hour, or evaluation failed — the caller's normal
/// inbound handling must continue in every case.
pub fn evaluate(store: &Store, dm: &InboundDm<'_>, now: i64) -> Option<String> {
    if !enabled() {
        return None;
    }
    match assess(store, dm, now) {
        Ok(id) => id,
        Err(error) => {
            warn!(platform = dm.platform, "dm alert evaluation failed: {error:#}");
            None
        }
    }
}

fn assess(store: &Store, dm: &InboundDm<'_>, now: i64) -> anyhow::Result<Option<String>> {
    let Some((selector, level)) = priority(store, dm)? else {
        return Ok(None);
    };
    let subject = subject(dm);
    if let Some(last) = last_alert_at(store, &subject)? {
        if now.saturating_sub(last) < WINDOW_MS {
            return Ok(None);
        }
    }
    let id = format!("{subject}:{now}");
    let label = match dm.sender_label.trim() {
        "" => dm.handles.first().map_or("", String::as_str),
        label => label,
    };
    let sender: String = label.chars().take(120).collect();
    let inserted = store.insert_owner_alert(
        &NewOwnerAlert {
            id: &id,
            source_url: dm.source_url,
            sender: &sender,
            action: &format!("Read {} message from {sender}", dm.platform),
            reason: &format!("Owner priority: {selector}"),
            urgency: level,
            due_at_ms: None,
            timezone: &timezone(),
            meeting_id: None,
            expires_at_ms: now + USEFUL_MS,
            text_after_ms: now,
        },
        now,
    )?;
    if !inserted {
        return Ok(None);
    }
    store.enqueue_owner_alert_texts(now)?;
    Ok(Some(id))
}

/// The owner priority covering this sender, if any. Mirrors the email gate's
/// precedence (exact beats person beats domain; a `routine` selector mutes),
/// so one owner configuration drives every channel.
fn priority(store: &Store, dm: &InboundDm<'_>) -> anyhow::Result<Option<(String, Urgency)>> {
    let handles: Vec<String> = if dm.handles.is_empty() {
        // A channel with no selector-shaped handle (a numeric platform id)
        // can still match a `person:` selector through its resolved slugs.
        vec![String::new()]
    } else {
        dm.handles.iter().map(|h| bare_sender(h)).collect()
    };
    let mut slugs = dm.person_slugs.to_vec();
    for handle in handles.iter().filter(|h| h.starts_with('+')) {
        if let Some(hit) = store.lookup_person_by_phone(handle)? {
            slugs.push(hit.person_slug);
        }
    }
    Ok(store
        .owner_alert_priorities()?
        .into_iter()
        .filter(|(selector, _)| {
            handles
                .iter()
                .any(|handle| selector_matches(selector, handle, &slugs))
        })
        .max_by_key(|(selector, urgency)| (specificity(selector), strength(*urgency)))
        .filter(|(_, urgency)| *urgency != Urgency::Routine))
}

fn specificity(selector: &str) -> u8 {
    if selector.starts_with("domain:") {
        1
    } else if selector.starts_with("person:") {
        2
    } else {
        3
    }
}
fn strength(urgency: Urgency) -> u8 {
    match urgency {
        Urgency::Routine => 3,
        Urgency::Critical => 2,
        Urgency::High => 1,
    }
}

/// Dedupe subject: the conversation identifier, digested so an alert id stays
/// inside the store's concise-notification limits and cannot pick up a `LIKE`
/// wildcard from a channel's own identifier shape.
fn subject(dm: &InboundDm<'_>) -> String {
    let digest = Sha256::digest(dm.conversation.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("dm:{}:{hex}", dm.platform)
}

fn last_alert_at(store: &Store, subject: &str) -> anyhow::Result<Option<i64>> {
    Ok(store.with_conn(|c| {
        c.query_row(
            "SELECT MAX(created_at_ms) FROM owner_alerts WHERE id LIKE ?1",
            [format!("{subject}:%")],
            |r| r.get::<_, Option<i64>>(0),
        )
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in contract: with [`ENABLED`] unset a qualifying DM must not
    /// alert. No other test in this binary sets the variable, so the
    /// default-off state is observable here without re-exec.
    #[test]
    fn evaluation_is_a_no_op_while_the_gate_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let chat = "15550000000@s.whatsapp.net";
        store.set_owner_alert_priority(chat, Urgency::High).unwrap();
        let handles = vec![chat.to_string()];
        let dm = InboundDm {
            platform: "whatsapp",
            conversation: chat,
            handles: &handles,
            person_slugs: &[],
            sender_label: "Priority Contact",
            source_url: "https://wa.me/15550000000",
        };
        assert!(evaluate(&store, &dm, 1_800_000_000_000).is_none());
    }
}
