//! D8/D9 — the closed action registry, its message templates, and the
//! deterministic content floor.
//!
//! | name                    | steps                          | reversible |
//! |-------------------------|--------------------------------|------------|
//! | `calendar.send_details` | 1 send (template)              | no         |
//! | `calendar.add_guest`    | 1 calendar PATCH + 1 send      | calendar step, until the guest accepts |
//! | `reply` (`full` only, not listable) | 1 send (drafter's text) | no |

/// Text the details of a calendar event back to the sender.
pub const SEND_DETAILS: &str = "calendar.send_details";
/// Add the contact's `calendar_email` as a guest, then text the details.
pub const ADD_GUEST: &str = "calendar.add_guest";
/// Send the existing drafter's free-form reply (`full` tier only).
pub const REPLY: &str = "reply";

/// Every registry action.
pub const ACTIONS: &[&str] = &[SEND_DETAILS, ADD_GUEST, REPLY];
/// Actions an `actions`-tier contact may list in `allowed_actions`.
pub const LISTABLE_ACTIONS: &[&str] = &[SEND_DETAILS, ADD_GUEST];

pub fn is_known(name: &str) -> bool {
    ACTIONS.contains(&name)
}

/// May appear in `allowed_actions`. `reply` is not listable: it comes only
/// with the `full` tier.
pub fn is_listable(name: &str) -> bool {
    let _ = name;
    todo!()
}

/// Whether the action has a step Undo can reverse.
pub fn is_reversible(name: &str) -> bool {
    let _ = name;
    todo!()
}

/// The event fields templates may use. Nothing else reaches the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFields {
    pub title: String,
    /// Already rendered in the owner's timezone.
    pub local_time: String,
    pub link: String,
}

/// `calendar.send_details`: `"{title} — {local_time}. Join: {link}"`.
pub fn send_details_text(event: &EventFields) -> String {
    let _ = event;
    todo!()
}

/// `calendar.add_guest`: `"Added you to {title} ({local_time}) — invite
/// sent to {calendar_email}. Join: {link}"`.
pub fn add_guest_text(event: &EventFields, calendar_email: &str) -> String {
    let _ = (event, calendar_email);
    todo!()
}

/// D9 content floor (OT1: the owner reviews this list in the C7 PR). Applies
/// to `full`-tier `reply` text only; templates cannot contain these. Each
/// entry is `(rule name, case-insensitive regex)`. Attachments on the
/// outbound reply are a separate structural check in the gate.
pub const CONTENT_FLOOR: &[(&str, &str)] = &[];

/// The first content-floor rule `text` hits, by name.
pub fn content_floor_hit(text: &str) -> Option<&'static str> {
    let _ = text;
    todo!()
}
