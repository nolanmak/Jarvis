//! D9 — per-channel 1:1 and owner-took-over rules.
//!
//! - iMessage: the thread is the DM keyed by the sender's own handle (not a
//!   `chat…`/group GUID), and the card names no other member. The owner
//!   took over when an `is_from_me` row (`fromEmail = 'me'`) was ingested
//!   into that thread after the inbound.
//! - Email: From is the contact, To ∪ Cc are only the owner's own
//!   addresses, no `List-Id`, and the reply envelope (when recorded) goes
//!   to the sender alone. The owner took over when a SENT message in the
//!   Gmail thread postdates the inbound (`outbound_thread_log`).

use augmentagent_channel_email::gmail::{extract_bare_email, split_recipients};
use augmentagent_store::{Email, Store};

use crate::handle::{normalize_handle, Handle};

/// `Ok(())` for a 1:1 thread; `Err(why)` otherwise (fail closed).
pub fn one_to_one(
    store: &Store,
    email: &Email,
    action_id: &str,
    sender: &Handle,
    extra_owner_addresses: &[String],
) -> Result<(), String> {
    let Some(thread) = email.thread_id.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Err("no thread id".into());
    };
    if email.platform == "imessage" {
        let id = thread
            .strip_prefix("imessage:")
            .ok_or("not an iMessage thread id")?;
        if id.starts_with("chat") || !email.to.trim().is_empty() {
            return Err("group conversation".into());
        }
        let conversation = normalize_handle("imessage", id)
            .map_err(|_| "group or unknown conversation".to_string())?;
        if &conversation != sender {
            return Err("the conversation is not the sender's own DM".into());
        }
        return Ok(());
    }
    // Email.
    if store
        .email_has_list_header(&email.message_id)
        .map_err(|e| format!("store: {e}"))?
    {
        return Err("mailing list (List-Id)".into());
    }
    let mut owner: Vec<String> = extra_owner_addresses
        .iter()
        .map(|a| a.trim().to_ascii_lowercase())
        .filter(|a| a.contains('@'))
        .collect();
    for account in store
        .get_active_gmail_accounts()
        .map_err(|e| format!("store: {e}"))?
    {
        if !account.email.trim().is_empty() {
            owner.push(account.email.trim().to_ascii_lowercase());
        }
        if let Some(e) = augmentagent_channel_email::google::email_of(&account.entity_id) {
            owner.push(e.trim().to_ascii_lowercase());
        }
    }
    let to = split_recipients(&email.to);
    if to.is_empty() {
        return Err("recipients unknown".into());
    }
    for r in to.iter().chain(split_recipients(&email.cc).iter()) {
        let bare = extract_bare_email(r).trim().to_ascii_lowercase();
        if !owner.contains(&bare) {
            return Err("other recipients on To/Cc".into());
        }
    }
    if let Some(env) = store
        .get_action_envelope(action_id)
        .map_err(|e| format!("store: {e}"))?
    {
        let only_sender = env.to.as_deref().is_none_or(|to| {
            let list = split_recipients(to);
            list.len() == 1
                && extract_bare_email(&list[0]).trim().to_ascii_lowercase() == sender.value()
        });
        let no_copies = [env.cc.as_deref(), env.bcc.as_deref()]
            .into_iter()
            .all(|v| v.is_none_or(|v| split_recipients(v).is_empty()));
        if !only_sender || !no_copies {
            return Err("the reply would go to someone besides the sender".into());
        }
    }
    Ok(())
}

/// D9 owner-took-over. `Err` when it cannot be established (callers fail
/// closed).
pub fn owner_took_over(store: &Store, email: &Email) -> Result<bool, String> {
    let thread = email.thread_id.as_deref().ok_or("no thread id")?;
    if email.platform == "imessage" {
        let inbound = chrono::DateTime::parse_from_rfc3339(email.date.trim())
            .map_err(|e| format!("inbound date: {e}"))?;
        let rows = store
            .owner_message_times_in_thread(thread, 500)
            .map_err(|e| format!("store: {e}"))?;
        return Ok(rows
            .iter()
            .any(|ts| chrono::DateTime::parse_from_rfc3339(ts.trim()).is_ok_and(|t| t > inbound)));
    }
    let inbound_ms = augmentagent_channel_email::outbound::parse_rfc2822_or_ms(&email.date);
    if inbound_ms <= 0 {
        return Err("inbound date unreadable".into());
    }
    store
        .thread_has_user_reply_after(thread, inbound_ms)
        .map_err(|e| format!("store: {e}"))
}
