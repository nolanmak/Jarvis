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
    LISTABLE_ACTIONS.contains(&name)
}

/// Whether the action has a step Undo can reverse.
pub fn is_reversible(name: &str) -> bool {
    name == ADD_GUEST
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
    format!(
        "{} — {}. Join: {}",
        event.title, event.local_time, event.link
    )
}

/// `calendar.add_guest`: `"Added you to {title} ({local_time}) — invite
/// sent to {calendar_email}. Join: {link}"`.
pub fn add_guest_text(event: &EventFields, calendar_email: &str) -> String {
    format!(
        "Added you to {} ({}) — invite sent to {calendar_email}. Join: {}",
        event.title, event.local_time, event.link
    )
}

/// D9 content floor (OT1: the owner reviews this list in the C7 PR). Applies
/// to `full`-tier `reply` text only; templates cannot contain these. Each
/// entry is `(rule name, case-insensitive regex)`. Attachments on the
/// outbound reply are a separate structural check in the gate.
pub const CONTENT_FLOOR: &[(&str, &str)] = &[
    (
        "money",
        r"\b(pay|pays|paid|paying|payment|payments|payout|invoice|invoices|wire|wired|transfer|transfers|venmo|zelle|paypal|cash\s?app|refund|refunds|reimburse\w*|deposit|deposits|owe|owed|loan|price|pricing|fee|fees|bank|banking|crypto|bitcoin|btc|eth|gift\s?cards?)\b",
    ),
    (
        "currency_amount",
        r"[$€£¥₹]\s?\d|\b\d[\d,]*(\.\d+)?\s?(usd|eur|gbp|cad|aud|dollars?|bucks|euros?|pounds?|k\b)",
    ),
    (
        "credentials",
        r"\b(password|passwords|passcode|passphrase|credential|credentials|login|log-in|username|api[\s_-]?keys?|secret|secrets|token|tokens|private\s+key|ssh\s+key|pin\s+(code|number))\b",
    ),
    (
        "verification_code",
        r"\b(2fa|mfa|otp|one[\s-]time\s+(pass)?code|verification\s+code|security\s+code|confirmation\s+code|auth(entication)?\s+code|login\s+code)\b",
    ),
    (
        "account_number",
        r"\b(routing|iban|swift\s+code|bic|sort\s+code|cvv|cvc|ssn|social\s+security)\b|\b(account|acct|card|credit\s+card|debit\s+card|bank\s+account)\s*(number|no\b|#|num\b)",
    ),
    ("long_number", r"\b\d{3}-\d{2}-\d{4}\b|\b\d{9,}\b"),
    ("attachment", r"\battach(ed|ment|ments|ing)?\b"),
];

fn compiled_floor() -> &'static [(&'static str, regex::Regex)] {
    static FLOOR: std::sync::OnceLock<Vec<(&'static str, regex::Regex)>> =
        std::sync::OnceLock::new();
    FLOOR.get_or_init(|| {
        CONTENT_FLOOR
            .iter()
            .map(|(name, pattern)| {
                let re = regex::RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .unwrap_or_else(|e| panic!("CONTENT_FLOOR rule {name} does not compile: {e}"));
                (*name, re)
            })
            .collect()
    })
}

/// The first content-floor rule `text` hits, by name.
pub fn content_floor_hit(text: &str) -> Option<&'static str> {
    compiled_floor()
        .iter()
        .find(|(_, re)| re.is_match(text))
        .map(|(name, _)| *name)
}
