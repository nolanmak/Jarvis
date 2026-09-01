//! Email-signature parsing → backfill role/title/company/phone (#64).
//!
//! `tone.rs::clean_sent_body` deliberately *discards* signature blocks (it
//! truncates at the RFC-3676 `\n-- \n` delimiter and the
//! `Sent from my …` trailer) because trailing sig text would distort the
//! voice profile. This module is the inverse: it **keeps** the sig block and
//! mines it for structured contact facts. It is fully additive — it never
//! calls into nor changes `tone.rs`, so the production tone pipeline stays
//! byte-identical (the zero-regression constraint).
//!
//! Pipeline (issue §"Proposed architecture"):
//! 1. [`strip_quoted_reply`] — drop the reply chain (a *local copy* of the
//!    quoted-reply rule, intentionally not shared with `tone.rs` so a future
//!    edit here can never regress the prod sent-mail cleaner).
//! 2. [`detect_signature_block`] — pure regex + line-density heuristic.
//!    Returns 0 or 1 candidate block. Unit-tested in isolation.
//! 3. [`SignatureExtractor`] — LLM field extraction over the existing
//!    `Reasoner` (Claude CLI) path: strict JSON with per-field confidence,
//!    retry-on-parse-fail, empty-stays-empty. A pure regex fallback covers
//!    the no-LLM / parse-fail path so the heuristic alone still yields
//!    high-confidence phones/URLs.
//! 4. [`signature_patch`] — fill-blanks-only wiki patch (shared merger).

use serde::{Deserialize, Serialize};

use augmentagent_channel_core::reasoner::{Reasoner, ReasonerOpts};
use augmentagent_wiki::PersonPatch;

/// Local-part patterns that mark a sender as non-human (newsletter, vendor,
/// automated). Compared case-insensitively against the part before `@` —
/// substring match, so `support+sub@…` and `team-marketing@…` both hit.
/// Kept in sync with `tone.rs::NOREPLY_LOCAL_PATTERNS` plus additional
/// CRM-only buckets (newsletter / marketing / billing local parts that
/// pollute `people/` per issue #120).
const NON_HUMAN_LOCAL_PATTERNS: &[&str] = &[
    "no-reply",
    "noreply",
    "donotreply",
    "do-not-reply",
    "notifications",
    "notification",
    "alerts",
    "alert",
    "mailer-daemon",
    "postmaster",
    "bounce",
    "support+",
    "news",
    "newsletter",
    "marketing",
    "deals",
    "updates",
    "digest",
    "billing",
    "receipts",
    "invoice",
    "team@",
    "team-",
    "hello@",
    "hi@",
    "info@",
    "contact@",
    "auto-confirm",
    "automated",
    "customerservice",
    "customer-service",
    // #451 — local-parts observed on mail that actually reached the approval
    // queue as a "reply-worthy" draft. An `offers@` sender was the one that
    // proved the list was too short.
    "offers",
    "offer@",
    "promo",
    "promotions",
    "sales@",
    "store@",
    "shop@",
    "orders",
    "email@",
    "mail@",
    "mailer",
    "subscriptions",
    "subscribe",
    "unsubscribe",
    "campaign",
    "broadcast",
    "announce",
    "events@",
    "community@",
    "membership",
];

/// #451 — sending *subdomains* that ESPs and retailers use for bulk mail.
/// Matched as a dotted label on the domain, so `e.nordstromrack.com` and
/// `engage.canva.com` are caught while a real company domain that merely
/// contains the letters (`email-security.io`) is not.
///
/// Anchored to a leading label to keep the match honest: we look for the
/// pattern as a full dot-delimited segment (`mail.acme.com`), never as a bare
/// substring (`gmail.com` must NOT match `mail`).
const BULK_SENDING_SUBDOMAINS: &[&str] = &[
    "e", "em", "mail", "email", "mkt", "marketing", "send", "sending", "engage", "news",
    "newsletter", "campaign", "campaigns", "notify", "notifications", "reply", "info", "go",
    "links", "link", "click", "cts", "message", "messages", "bulk", "blast",
];

/// Domain suffixes / substrings owned by ESPs and bulk senders. Mail
/// originating from these domains is almost never a human counterpart.
/// Substring match against the lowercased domain.
const NON_HUMAN_DOMAIN_PATTERNS: &[&str] = &[
    "mailgun",
    "sendgrid",
    "postmarkapp",
    "resend.dev",
    "mailchimp",
    "mailerlite",
    "constantcontact",
    "amazonses",
    "sparkpost",
    "convertkit",
    "substack.com",
    "beehiiv.com",
    "buttondown.email",
    "mc.notifylink.com",
    "linkedin.com",
    "github.com",
    "gitlab.com",
    "atlassian.com",
    "uber.com",
    "venmo.com",
    "chase.com",
    "wellsfargo.com",
    "amazon.com",
    "stripe.com",
    "coderabbit",
    "dependabot",
];

/// Body markers (case-insensitive) that strongly indicate a bulk / automated
/// mail rather than a human conversation: List-Unsubscribe headers leaking
/// into the body, "click here to unsubscribe" boilerplate, "do not reply"
/// disclaimers.
const NON_HUMAN_BODY_MARKERS: &[&str] = &[
    "list-unsubscribe:",
    "precedence: bulk",
    "auto-submitted: auto-",
    "this is an automated",
    "do not reply to this email",
    "this email was sent automatically",
    "click here to unsubscribe",
    "manage your subscription",
    "manage your preferences",
    "view this email in your browser",
];

/// Heuristic classifier: should this `(from, body)` pair create a new
/// `people/` CRM page? Returns `true` only when the sender looks like a
/// human counterpart (not a newsletter, vendor, or no-reply bot).
///
/// Used to gate **new-page creation** in the email-signature backfill so
/// `people/` doesn't accumulate hundreds of subscription / SaaS / billing
/// entries (issue #120). Existing pages are left untouched — they may
/// already be valuable to the user; cleanup of the polluted backlog is a
/// follow-up, not gated by this helper.
///
/// Decision order:
/// 1. Body markers (List-Unsubscribe, "do not reply", etc.) → not human.
/// 2. Local-part patterns (`noreply`, `newsletter`, `support+`, …) → not human.
/// 3. ESP / bulk-sender domain patterns (Mailgun, Substack, …) → not human.
/// 4. Otherwise: assume human (default-allow, so we never *block* a real
///    person who happens to have an unusual mailbox).
pub fn is_human_sender(from: &str, body: &str) -> bool {
    let bare = extract_bare(from);
    if bare.is_empty() {
        return false;
    }
    let lower = bare.to_ascii_lowercase();
    let (local, domain) = match lower.split_once('@') {
        Some((l, d)) => (l, d),
        // No `@` at all is not a real address.
        None => return false,
    };

    // 1. Body-content markers (bulk-mail signature).
    if !body.is_empty() {
        let body_lower = body.to_ascii_lowercase();
        for marker in NON_HUMAN_BODY_MARKERS {
            if body_lower.contains(marker) {
                return false;
            }
        }
    }

    // 2. Local-part patterns. Use `contains` so `support+foo` and
    //    `team-news` both hit; pattern strings that include `@` or `-`
    //    are matched as anchors against `local@` synthesized below so
    //    `team@` doesn't false-positive on `team-lead@`.
    let local_at = format!("{local}@");
    for pat in NON_HUMAN_LOCAL_PATTERNS {
        if pat.ends_with('@') || pat.ends_with('-') {
            if local_at.contains(pat) || local.contains(pat) {
                return false;
            }
        } else if local.contains(pat) {
            return false;
        }
    }

    // 3. Known ESP / bulk-sender domains.
    for pat in NON_HUMAN_DOMAIN_PATTERNS {
        if domain.contains(pat) {
            return false;
        }
    }

    // 4. #451 — bulk sending subdomains. A real person does not write to you
    //    from `e.nordstromrack.com` or `engage.canva.com`. Compare whole
    //    dot-delimited labels, and only the leading ones: the last two labels
    //    are the registrable domain (`nordstromrack.com`), and a company whose
    //    apex happens to be `mail.com` or `news.com` is a real counterpart, not
    //    a blast. `gmail.com` therefore cannot match `mail`, because `gmail` is
    //    a single label and is part of the apex.
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() > 2 {
        let leading = &labels[..labels.len() - 2];
        for label in leading {
            if BULK_SENDING_SUBDOMAINS.contains(label) {
                return false;
            }
        }
    }

    true
}

/// Domains owned by event platforms / signup-confirmation senders. Matched
/// against the lowercased `domain` portion of the bare address. Plain
/// substring match unless the pattern is anchored:
///   - leading `.` (e.g. `.luma-mail.com`) → suffix match (true subdomains
///     of `luma-mail.com` but NOT `luma-mail.com.evil.example`).
/// Curated for the senders the user signed up to via a script — Partiful,
/// Luma, Meetup, Eventbrite, Covent, Hopin, Zoom, plus generic
/// `noreply@calendar.<corp>` (handled separately on local-part below).
const EVENT_BLAST_DOMAIN_PATTERNS: &[&str] = &[
    "partiful-mail.com",
    "email.meetup.com",
    "eventbrite.com",
    "joincovent.com",
    "hopin.com",
    "zoom.us",
    ".luma-mail.com",
];

/// Subject markers (case-insensitive) for event invites / RSVPs /
/// registration confirmations. Plain `contains` substring match against the
/// lowercased subject. The weekday list is enumerated explicitly so we can
/// match "see you wed" / "see you next thursday" without pulling in a regex
/// crate.
const EVENT_BLAST_SUBJECT_MARKERS: &[&str] = &[
    "registration confirmed",
    "you're invited",
    "youre invited",
    "you're in!",
    "youre in!",
    "rsvp",
    "invitation from an unknown sender",
    "action required: complete your registration",
    "\u{1f48c}", // 💌 (love letter emoji used by invite platforms)
    "your tickets",
];

/// Weekday names — combined with the literal prefix "see you" to match
/// "See you Wed", "See you next Thursday", "See you this Friday", etc.
/// Lowercase; matched against the lowercased subject as a substring search
/// for `"see you " + (?optional "next "|"this ") + weekday`.
const WEEKDAYS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "mon",
    "tue",
    "tues",
    "wed",
    "thu",
    "thur",
    "thurs",
    "fri",
    "sat",
    "sun",
];

/// Body markers (case-insensitive) that strongly indicate an event invite /
/// calendar-attached blast — a calendar artifact in the body or an `.ics`
/// attachment reference.
const EVENT_BLAST_BODY_MARKERS: &[&str] = &[
    "add to calendar",
    ".ics attached",
    "ical://",
    "google.com/calendar",
];

/// Heuristic: should this `(from, subject, body)` be treated as an
/// event-platform / signup-confirmation blast?
///
/// Issue #222: the user signed up for many tech events via a script and
/// doesn't want a draft reply for each Partiful/Luma/Meetup/Eventbrite
/// invite, RSVP confirmation, or "Action Required: Complete Registration"
/// nudge. These mails carry valuable CRM context (organizers, event
/// titles, locations), so we still ingest into the wiki — but we never
/// generate a draft, never queue an approval card, and never post a
/// Discord flag notice (per the user's explicit preference: no noise from
/// event blasts).
///
/// Returns true if **any** of:
///   1. The sender domain matches a curated event-platform list.
///   2. The local part is `noreply@calendar.<anything>` (corporate calendar
///      invites — most Calendly / Google Calendar bounce-back domains
///      follow this shape).
///   3. The subject contains a registration/invite/RSVP marker — including
///      "see you (next|this)? <weekday>" via the explicit weekday list.
///   4. The body references calendar boilerplate (`Add to calendar`,
///      `.ics attached`, `ical://`, `google.com/calendar`).
///
/// All comparisons are case-insensitive plain `contains` checks; no regex
/// crate dependency.
pub fn is_event_blast(from: &str, subject: &str, body: &str) -> bool {
    // 1 + 2. Sender — domain list + `noreply@calendar.` local-part rule.
    let bare = extract_bare(from).to_ascii_lowercase();
    if let Some((local, domain)) = bare.split_once('@') {
        for pat in EVENT_BLAST_DOMAIN_PATTERNS {
            if let Some(suffix) = pat.strip_prefix('.') {
                // Anchored suffix match: only true subdomains of `<suffix>`.
                if domain.ends_with(suffix) && domain != suffix {
                    return true;
                }
            } else if domain.contains(pat) {
                return true;
            }
        }
        // `noreply@calendar.<anything>` — corporate calendar invites.
        if local.starts_with("noreply") && domain.starts_with("calendar.") {
            return true;
        }
    }

    // 3. Subject markers (case-insensitive substring).
    let subject_lower = subject.to_ascii_lowercase();
    for marker in EVENT_BLAST_SUBJECT_MARKERS {
        if subject_lower.contains(marker) {
            return true;
        }
    }
    // 3b. "see you (next|this)? <weekday>" — enumerated weekday match.
    if subject_lower.contains("see you ") {
        for wd in WEEKDAYS {
            // Direct: "see you wed"
            // Modified: "see you next wed" / "see you this wed"
            for prefix in ["see you ", "see you next ", "see you this "] {
                let needle = format!("{prefix}{wd}");
                if subject_lower.contains(&needle) {
                    return true;
                }
            }
        }
    }

    // 4. Body markers.
    if !body.is_empty() {
        let body_lower = body.to_ascii_lowercase();
        for marker in EVENT_BLAST_BODY_MARKERS {
            if body_lower.contains(marker) {
                return true;
            }
        }
    }

    false
}

/// Subject prefixes stripped (repeatedly, since forwards stack) before the
/// invite-subject markers are matched. Lowercase; compared against the
/// lowercased, trimmed subject.
const SUBJECT_FORWARD_PREFIXES: &[&str] = &["fw:", "fwd:", "re:"];

/// Subject markers (lowercase) that calendar clients — Google Calendar,
/// Outlook, Teams — put at the START of an invite / update / RSVP subject.
/// Matched with `starts_with` after forward prefixes are stripped, so an
/// ordinary mail merely mentioning "invitation" mid-subject won't match.
const MEETING_INVITE_SUBJECT_PREFIXES: &[&str] = &[
    "invitation:",
    "updated invitation:",
    "canceled event:",
    "accepted:",
    "tentatively accepted:",
    "declined:",
    "meeting canceled:",
    "meeting updated:",
];

/// Teams join markers. Any one of these, *combined with* the literal
/// "microsoft teams meeting", identifies a real invite body — prose that
/// merely proposes a Teams call carries neither.
const TEAMS_JOIN_MARKERS: &[&str] = &[
    "join the meeting now",
    "teams.microsoft.com/l/meetup-join",
    "meeting id:",
];

/// Conferencing join URLs, matched anywhere in the body. Provider-agnostic
/// so a forwarded Zoom / Meet / Webex invite is caught on the same footing
/// as the Teams one that was reported.
const JOIN_LINK_MARKERS: &[&str] = &[
    "zoom.us/j/",
    "zoom.us/my/",
    "meet.google.com/",
    "teams.microsoft.com/l/meetup-join",
    "teams.live.com/meet/",
    ".webex.com/",
    "whereby.com/",
    "chime.aws/",
    "meet.jit.si/",
];

/// Line-anchored headers that calendar clients print as a block above a
/// forwarded invite. Any TWO distinct ones mean an invite; one alone (a
/// stray "Where: the usual spot" in prose) does not.
const INVITE_HEADER_PREFIXES: &[&str] = &[
    "when:",
    "where:",
    "organizer:",
    "attendees:",
    "required attendees:",
    "optional attendees:",
    "invitees:",
];

/// Heuristic: is this `(subject, body, attachments)` a meeting / calendar
/// invite rather than something to reply to?
///
/// Issue #834: triage drafted a full "can you confirm attendance, and could
/// you share an agenda?" reply to a forwarded Microsoft Teams invite. The
/// sender was human (so #217's `is_human_sender` passed) and the mail was
/// nothing like an event-platform blast (so #222's `is_event_blast` missed
/// it) — the user's position is that we never draft responses to meeting
/// invites at all. Callers route a match to the Flag path: the invite is
/// still surfaced for calendar handling, but no draft is generated.
///
/// Returns true if **any** of:
///   1. An attachment label names a calendar artifact (`text/calendar` MIME
///      or a `.ics` filename — matched at a token end so it reads as an
///      extension, not any substring of a longer label). Bonus signal only:
///      `list_retryable_replies` rehydrates `Email` with no attachments, so
///      the body/subject rules below carry the persisted path on their own.
///   2. The body is a Teams invite: "Microsoft Teams meeting" AND a join
///      marker. Conjunctive on purpose — "let's do a Teams meeting" must
///      still get a draft.
///   3. The body pairs a line-anchored `Join ...` lead-in with a
///      conferencing join URL — the provider-agnostic online-invite shape
///      ("Join Zoom Meeting" over a `zoom.us/j/` link, "Join with Google
///      Meet" over a `meet.google.com/` link). Conjunctive so prose that
///      merely pastes a standing room link still gets a draft.
///   4. The body carries two distinct line-anchored invite headers
///      (`When:` / `Where:` / `Organizer:` / `Attendees:` / …) — the
///      forwarded Outlook/Teams shape, including the online-meeting variant
///      that drops `Where:`. Anchored per-line so a mid-sentence "when:"
///      can't trip it, and paired so one stray header can't either.
///   5. The body carries Google Calendar's invitation boilerplate.
///   6. The subject, after stripping stacked `Fw:`/`Fwd:`/`Re:` prefixes,
///      starts with a calendar-client invite marker.
///
/// All comparisons are case-insensitive plain string ops; no regex crate.
pub fn is_meeting_invite(subject: &str, body: &str, attachments: &[String]) -> bool {
    // 1. Calendar attachment labels (e.g. `invite.ics (text/calendar)`).
    for att in attachments {
        let att_lower = att.to_ascii_lowercase();
        if att_lower.contains("text/calendar")
            || att_lower.split_whitespace().any(|t| t.ends_with(".ics"))
        {
            return true;
        }
    }

    if !body.is_empty() {
        let body_lower = body.to_ascii_lowercase();

        // 2. Teams invite body — product name AND a join marker.
        if body_lower.contains("microsoft teams meeting")
            && TEAMS_JOIN_MARKERS.iter().any(|m| body_lower.contains(m))
        {
            return true;
        }

        // 3. Generic online invite — a `Join ...` line AND a provider URL.
        if body_lower
            .lines()
            .any(|l| l.trim_start().starts_with("join "))
            && JOIN_LINK_MARKERS.iter().any(|m| body_lower.contains(m))
        {
            return true;
        }

        // 4. Two distinct line-anchored invite headers.
        let mut seen_headers = 0u32;
        for line in body_lower.lines() {
            let line = line.trim_start();
            if let Some(i) = INVITE_HEADER_PREFIXES
                .iter()
                .position(|h| line.starts_with(h))
            {
                seen_headers |= 1 << i;
            }
        }
        if seen_headers.count_ones() >= 2 {
            return true;
        }

        // 5. Google Calendar boilerplate.
        if body_lower.contains("invitation from google calendar") {
            return true;
        }
    }

    // 6. Subject markers, with stacked forward prefixes stripped.
    let mut subject_lower = subject.trim().to_ascii_lowercase();
    loop {
        let stripped = SUBJECT_FORWARD_PREFIXES
            .iter()
            .find_map(|p| subject_lower.strip_prefix(p));
        match stripped {
            Some(rest) => subject_lower = rest.trim_start().to_string(),
            None => break,
        }
    }
    MEETING_INVITE_SUBJECT_PREFIXES
        .iter()
        .any(|m| subject_lower.starts_with(m))
}

/// Pull the bare `local@domain` from a raw `From:` header value that may
/// include a display name (`"Foo" <foo@bar.com>` or `Foo <foo@bar.com>`). // pii-ok
/// Falls back to the trimmed input if no angle-bracket pattern is found.
///
/// Uses the LAST `<addr>` pair: RFC 5322 puts the addr-spec after the display
/// name, so a spoofed bracketed fragment inside the display name (e.g.
/// `"<a@gmail.com>" <noreply@stripe.com>`) must not shadow the real address // pii-ok
/// and bypass the human-sender / event-blast gates (#253).
fn extract_bare(from: &str) -> String {
    let trimmed = from.trim();
    if let Some(start) = trimmed.rfind('<') {
        if let Some(end) = trimmed[start + 1..].find('>') {
            return trimmed[start + 1..start + 1 + end].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Strip a Gmail/Outlook quoted-reply chain. **Local copy** of the rule in
/// `tone.rs` (lines flagged "Strip Gmail's quoted-reply preamble"): keeping
/// it separate is deliberate — #64 must not edit the prod tone cleaner.
/// Unlike `clean_sent_body`, this stops *before* the sig delimiter so the
/// signature survives for extraction.
pub fn strip_quoted_reply(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n");
    let s = if let Some(on_pos) = normalized.find("\nOn ") {
        if normalized[on_pos..].contains(" wrote:") {
            normalized[..on_pos].to_string()
        } else {
            normalized
        }
    } else {
        normalized
    };
    // Drop `>`-quoted lines but DO keep the sig delimiter + sig body.
    s.lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A detected signature candidate: the raw block text + why we think it's a
/// signature (for the dry-run report / debugging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureBlock {
    pub text: String,
    /// `delimiter` (explicit `-- `), `closing` (Best,/Regards,/Thanks,),
    /// or `density` (trailing low-word-count lines with contact tokens).
    pub reason: &'static str,
}

/// Closing salutations that mark the start of a sig block when followed by a
/// short name/contact tail.
const CLOSINGS: &[&str] = &[
    "best,", "best regards,", "regards,", "kind regards,", "warm regards,",
    "thanks,", "thank you,", "cheers,", "sincerely,", "best wishes,",
    "talk soon,", "warmly,", "respectfully,",
];

/// Mobile-mail trailers — their own (1-line) signature.
const MOBILE_TRAILERS: &[&str] = &[
    "sent from my iphone",
    "sent from my ipad",
    "sent from my android",
    "sent from my mobile",
    "sent from my galaxy",
    "get outlook for",
];

/// Detect 0 or 1 signature block in a (quoted-reply-stripped) body.
///
/// Strategy, highest-confidence first:
/// 1. Explicit RFC-3676 delimiter line `-- ` → everything after is the sig.
/// 2. A closing salutation line near the end → sig starts there.
/// 3. Line-density: the trailing run of short lines (≤ ~6 words) that carry
///    contact tokens (phone digits, URL, `@`, title words).
///
/// Returns `None` when nothing convincing is found (no false sig on a plain
/// prose email — empty stays empty downstream).
pub fn detect_signature_block(body: &str) -> Option<SignatureBlock> {
    let body = body.trim_end();
    if body.is_empty() {
        return None;
    }
    let lines: Vec<&str> = body.lines().collect();

    // 1. Explicit delimiter.
    for (i, l) in lines.iter().enumerate() {
        if l.trim_end() == "--" || l.trim_end() == "-- " || l.trim() == "--" {
            let block = lines[i + 1..].join("\n").trim().to_string();
            if !block.is_empty() {
                return Some(SignatureBlock {
                    text: block,
                    reason: "delimiter",
                });
            }
        }
    }

    // 2. Mobile trailer (anywhere in the last 3 lines).
    let tail_start = lines.len().saturating_sub(3);
    for (i, l) in lines.iter().enumerate().skip(tail_start) {
        let low = l.trim().to_ascii_lowercase();
        if MOBILE_TRAILERS.iter().any(|t| low.starts_with(t)) {
            return Some(SignatureBlock {
                text: lines[i..].join("\n").trim().to_string(),
                reason: "delimiter",
            });
        }
    }

    // 3. Closing salutation in the last ~8 lines.
    let window_start = lines.len().saturating_sub(8);
    for (i, l) in lines.iter().enumerate().skip(window_start) {
        let low = l.trim().to_ascii_lowercase();
        if CLOSINGS.iter().any(|c| low == *c || low.starts_with(c)) {
            // Sig is the closing + everything after; require at least one
            // contact-ish or name line after the closing so "Thanks," at the
            // end of a sentence-less line isn't a false positive.
            let after = &lines[i + 1..];
            if after.iter().any(|x| looks_like_contact(x) || is_name_line(x)) {
                let block = lines[i..].join("\n").trim().to_string();
                return Some(SignatureBlock {
                    text: block,
                    reason: "closing",
                });
            }
        }
    }

    // 4. Trailing low-density run carrying contact tokens.
    let mut start = lines.len();
    let mut contact_hits = 0;
    for i in (0..lines.len()).rev() {
        let l = lines[i].trim();
        if l.is_empty() {
            if lines.len() - i > 6 {
                break;
            }
            continue;
        }
        let words = l.split_whitespace().count();
        if words <= 7 && (looks_like_contact(l) || is_name_line(l)) {
            if looks_like_contact(l) {
                contact_hits += 1;
            }
            start = i;
        } else {
            break;
        }
    }
    if start < lines.len() && contact_hits >= 1 && lines.len() - start <= 8 {
        let block = lines[start..].join("\n").trim().to_string();
        if !block.is_empty() {
            return Some(SignatureBlock {
                text: block,
                reason: "density",
            });
        }
    }

    None
}

/// A line that carries a contact token: phone-ish digit run, URL, email,
/// or a known title keyword.
fn looks_like_contact(line: &str) -> bool {
    let l = line.trim();
    if l.is_empty() {
        return false;
    }
    let low = l.to_ascii_lowercase();
    let digits = l.chars().filter(|c| c.is_ascii_digit()).count();
    let phoneish = digits >= 7
        && l.chars()
            .all(|c| c.is_ascii_digit() || " +-().x".contains(c) || c == '\t');
    phoneish
        || low.contains("http://")
        || low.contains("https://")
        || low.contains("www.")
        || (l.contains('@') && l.contains('.') && !l.contains(' '))
        || TITLE_WORDS.iter().any(|w| low.contains(w))
}

const TITLE_WORDS: &[&str] = &[
    "ceo", "cto", "cfo", "coo", "founder", "co-founder", "engineer",
    "developer", "manager", "director", "president", "vp ", "head of",
    "lead", "partner", "consultant", "designer", "analyst", "officer",
    "principal", "architect",
];

/// A short, capitalized, punctuation-light line — likely a person/company
/// name in a sig.
fn is_name_line(line: &str) -> bool {
    let l = line.trim();
    if l.is_empty() || l.len() > 60 {
        return false;
    }
    let words: Vec<&str> = l.split_whitespace().collect();
    if words.is_empty() || words.len() > 6 {
        return false;
    }
    let cap = words
        .iter()
        .filter(|w| w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .count();
    let has_sentence_punct = l.ends_with('.') && l.contains(". ");
    cap * 2 >= words.len() && !has_sentence_punct
}

/// Strict JSON the LLM must return. Every field optional; `phones` a list;
/// each field carries an independent confidence in `confidence`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtractedFields {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub phones: Vec<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub socials: Socials,
    /// Per-field 0.0–1.0 confidence; missing key ⇒ treated as 0.
    #[serde(default)]
    pub confidence: std::collections::BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Socials {
    #[serde(default)]
    pub linkedin: Option<String>,
    #[serde(default)]
    pub twitter: Option<String>,
    #[serde(default)]
    pub github: Option<String>,
}

impl ExtractedFields {
    pub fn is_empty(&self) -> bool {
        self.role.is_none()
            && self.title.is_none()
            && self.company.is_none()
            && self.phones.is_empty()
            && self.address.is_none()
            && self.website.is_none()
            && self.socials == Socials::default()
    }

    /// Confidence for `field` (0 if absent).
    pub fn conf(&self, field: &str) -> f64 {
        self.confidence.get(field).copied().unwrap_or(0.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SigError {
    #[error("reasoner: {0}")]
    Reasoner(String),
    #[error("parse: model did not return JSON after retry")]
    Parse,
}

/// Drives the LLM extraction over the shared `Reasoner` seam. Tests inject a
/// fake reasoner; prod uses `ClaudeCliReasoner`.
pub struct SignatureExtractor<'a> {
    pub reasoner: &'a dyn Reasoner,
}

const SYS_PROMPT: &str = "You extract structured contact facts from an email \
signature block. Return ONLY a single JSON object, no prose, no code fence. \
Schema: {\"role\":string|null,\"title\":string|null,\"company\":string|null,\
\"phones\":[string],\"address\":string|null,\"website\":string|null,\
\"socials\":{\"linkedin\":string|null,\"twitter\":string|null,\"github\":string|null},\
\"confidence\":{\"role\":0..1,\"title\":0..1,\"company\":0..1,\"phones\":0..1,\
\"address\":0..1,\"website\":0..1}}. NEVER invent a value that is not present \
in the block — use null. Confidence reflects how certain you are the value \
is correct AND present.";

impl<'a> SignatureExtractor<'a> {
    pub fn new(reasoner: &'a dyn Reasoner) -> Self {
        Self { reasoner }
    }

    /// Extract fields from a detected sig block. One retry on parse failure
    /// (the prompt re-states "JSON only"); then a pure-regex fallback so we
    /// still capture high-confidence phones/URLs without the LLM.
    pub async fn extract(&self, block: &str) -> Result<ExtractedFields, SigError> {
        let opts = ReasonerOpts {
            system_prompt: SYS_PROMPT.to_string(),
            model: None,
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".to_string(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
        };

        for attempt in 0..2 {
            let user = if attempt == 0 {
                format!("Signature block:\n```\n{block}\n```")
            } else {
                format!(
                    "Return ONLY the JSON object (no fence, no prose). \
                     Signature block:\n```\n{block}\n```"
                )
            };
            match self.reasoner.call(&opts, &user).await {
                Ok(out) => {
                    if let Some(parsed) = parse_json_lenient(&out) {
                        return Ok(merge_with_regex(parsed, block));
                    }
                }
                Err(e) => {
                    // Network/spawn failure: don't burn a retry forever.
                    if attempt == 1 {
                        return Err(SigError::Reasoner(e.to_string()));
                    }
                }
            }
        }
        // Fallback: heuristics-only (never empty-invents; only what's literally
        // in the block).
        let rx = regex_only(block);
        if rx.is_empty() {
            Err(SigError::Parse)
        } else {
            Ok(rx)
        }
    }
}

/// Accept raw JSON, a ```json fenced block, or JSON with leading/trailing
/// prose — extract the first balanced `{...}` and parse it.
pub fn parse_json_lenient(s: &str) -> Option<ExtractedFields> {
    let s = s.trim();
    if let Ok(v) = serde_json::from_str::<ExtractedFields>(s) {
        return Some(v);
    }
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s[start..].char_indices() {
        match c {
            '"' if !esc => in_str = !in_str,
            '\\' if in_str => {
                esc = !esc;
                continue;
            }
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    let cand = &s[start..start + i + 1];
                    return serde_json::from_str::<ExtractedFields>(cand).ok();
                }
            }
            _ => {}
        }
        esc = false;
    }
    None
}

/// Fill any field the LLM left null but the regex pass found verbatim in the
/// block — strictly additive (never overrides an LLM value), and only for
/// objectively-present tokens (phones / URLs), so "empty stays empty" holds.
fn merge_with_regex(mut f: ExtractedFields, block: &str) -> ExtractedFields {
    let rx = regex_only(block);
    if f.phones.is_empty() && !rx.phones.is_empty() {
        f.phones = rx.phones;
        f.confidence.entry("phones".into()).or_insert(0.9);
    }
    if f.website.is_none() && rx.website.is_some() {
        f.website = rx.website;
        f.confidence.entry("website".into()).or_insert(0.9);
    }
    if f.socials.linkedin.is_none() {
        f.socials.linkedin = rx.socials.linkedin;
    }
    f
}

/// Pure regex/heuristic extraction of the objectively-present tokens:
/// phone-like digit runs, URLs, a LinkedIn handle. High confidence by
/// construction (they're literally in the text). Never guesses role/company.
pub fn regex_only(block: &str) -> ExtractedFields {
    let mut f = ExtractedFields::default();
    for line in block.lines() {
        let l = line.trim();
        // Phone: a line that is mostly digits + phone punctuation.
        let digits = l.chars().filter(|c| c.is_ascii_digit()).count();
        if (7..=15).contains(&digits)
            && l.chars().all(|c| {
                c.is_ascii_digit() || " +-().x/\t".contains(c)
            })
        {
            let normalized: String = l
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '+')
                .collect();
            if !f.phones.contains(&normalized) {
                f.phones.push(normalized);
            }
        }
        // URL / website.
        for tok in l.split_whitespace() {
            let t = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != ':' && c != '.');
            let low = t.to_ascii_lowercase();
            if low.contains("linkedin.com/in/") || low.contains("linkedin.com/company/") {
                f.socials.linkedin.get_or_insert_with(|| t.to_string());
            } else if (low.starts_with("http://")
                || low.starts_with("https://")
                || low.starts_with("www."))
                && f.website.is_none()
            {
                f.website = Some(t.to_string());
            }
        }
    }
    if !f.phones.is_empty() {
        f.confidence.insert("phones".into(), 0.92);
    }
    if f.website.is_some() {
        f.confidence.insert("website".into(), 0.9);
    }
    f
}

/// Build a fill-blanks wiki patch from extracted fields. Only fields whose
/// confidence ≥ `min_conf` are written; lower-confidence ones are returned
/// separately so the caller can batch them into the daily Discord approval
/// digest (issue: "low-confidence → daily Discord digest").
pub fn signature_patch(
    fields: &ExtractedFields,
    today: &str,
    min_conf: f64,
) -> (PersonPatch, Vec<String>) {
    let mut p = PersonPatch::new()
        .source(format!("Email-signature extraction on {today}"));
    let mut deferred: Vec<String> = Vec::new();

    let consider = |key: &str,
                        label: &str,
                        val: Option<&str>,
                        p: &mut PersonPatch,
                        deferred: &mut Vec<String>| {
        if let Some(v) = val.map(str::trim).filter(|v| !v.is_empty()) {
            if fields.conf(key) >= min_conf {
                *p = std::mem::take(p).profile_row(label, v);
            } else {
                deferred.push(format!("{label}: {v} (conf {:.2})", fields.conf(key)));
            }
        }
    };

    consider("title", "Title", fields.title.as_deref(), &mut p, &mut deferred);
    consider("role", "Role", fields.role.as_deref(), &mut p, &mut deferred);
    consider(
        "company",
        "Company",
        fields.company.as_deref(),
        &mut p,
        &mut deferred,
    );
    consider(
        "address",
        "Address",
        fields.address.as_deref(),
        &mut p,
        &mut deferred,
    );
    consider(
        "website",
        "Website",
        fields.website.as_deref(),
        &mut p,
        &mut deferred,
    );

    if fields.conf("phones") >= min_conf {
        for ph in &fields.phones {
            if !ph.trim().is_empty() {
                p = p.identity("phone", ph.trim());
            }
        }
        if let Some(first) = fields.phones.first() {
            p = p.profile_row("Phone", first.trim());
        }
    } else if !fields.phones.is_empty() {
        deferred.push(format!(
            "Phone: {} (conf {:.2})",
            fields.phones.join(", "),
            fields.conf("phones")
        ));
    }

    if let Some(li) = fields.socials.linkedin.as_deref() {
        // A LinkedIn URL in a sig is objectively present → always safe.
        p = p.profile_row("LinkedIn URL", li);
    }

    (p, deferred)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[test]
    fn strip_quoted_reply_keeps_signature() {
        let raw = "Hey, sounds good.\n\n-- \nJane Doe\nCTO, Acme\n+1 415 555 0100\n\nOn Mon, X wrote:\n> old stuff\n";
        let s = strip_quoted_reply(raw);
        assert!(s.contains("-- "));
        assert!(s.contains("Jane Doe"));
        assert!(!s.contains("old stuff"));
        assert!(!s.contains("On Mon"));
    }

    #[test]
    fn detects_explicit_delimiter_block() {
        let body = "Thanks for the update.\n\n-- \nJane Doe\nCTO at Acme\n+1 415 555 0100\nhttps://acme.com";
        let b = detect_signature_block(body).unwrap();
        assert_eq!(b.reason, "delimiter");
        assert!(b.text.starts_with("Jane Doe"));
        assert!(!b.text.contains("Thanks for the update"));
    }

    #[test]
    fn detects_closing_salutation_block() {
        let body = "Let's sync next week about the roadmap.\n\nBest,\nJane Doe\nVP Engineering, Acme\n+1 (415) 555-0100";
        let b = detect_signature_block(body).unwrap();
        assert_eq!(b.reason, "closing");
        assert!(b.text.contains("Jane Doe"));
    }

    #[test]
    fn detects_mobile_trailer() {
        let body = "yep works for me\n\nSent from my iPhone";
        let b = detect_signature_block(body).unwrap();
        assert!(b.text.to_lowercase().contains("sent from my iphone"));
    }

    #[test]
    fn no_signature_on_plain_prose() {
        let body = "Hi team,\n\nThe deploy went out at noon and metrics look healthy. \
                    I'll keep watching the error rate through the afternoon and report back \
                    if anything regresses. Let me know if you need anything else.";
        assert!(detect_signature_block(body).is_none());
    }

    #[test]
    fn density_block_with_phone() {
        let body = "Approved — ship it.\n\nJane Doe\nAcme Corp\n415-555-0100\njane@acme.com";
        let b = detect_signature_block(body).unwrap();
        assert!(b.text.contains("415-555-0100"));
    }

    #[test]
    fn regex_only_pulls_phone_and_url() {
        let block = "Jane Doe\nCTO\n+1 (415) 555-0100\nhttps://acme.com\nlinkedin.com/in/janedoe";
        let f = regex_only(block);
        assert_eq!(f.phones, vec!["+14155550100"]);
        assert_eq!(f.website.as_deref(), Some("https://acme.com"));
        assert!(f
            .socials
            .linkedin
            .as_deref()
            .unwrap()
            .contains("linkedin.com/in/janedoe"));
        assert!(f.conf("phones") > 0.9);
    }

    #[test]
    fn parse_json_lenient_handles_fence_and_prose() {
        let raw = "Sure! Here is the JSON:\n```json\n{\"company\":\"Acme\",\"phones\":[\"+1415\"],\"confidence\":{\"company\":0.8}}\n```\nHope that helps.";
        let f = parse_json_lenient(raw).unwrap();
        assert_eq!(f.company.as_deref(), Some("Acme"));
        assert_eq!(f.conf("company"), 0.8);
    }

    #[test]
    fn signature_patch_gates_low_confidence() {
        let mut f = ExtractedFields {
            company: Some("Acme".into()),
            title: Some("CTO".into()),
            ..Default::default()
        };
        f.confidence.insert("company".into(), 0.95);
        f.confidence.insert("title".into(), 0.40);
        let (patch, deferred) = signature_patch(&f, "2026-05-18", 0.7);
        assert!(patch.profile.iter().any(|(k, v)| k == "Company" && v == "Acme"));
        assert!(!patch.profile.iter().any(|(k, _)| k == "Title"));
        assert_eq!(deferred.len(), 1);
        assert!(deferred[0].contains("CTO"));
    }

    struct FakeReasoner {
        replies: Mutex<Vec<Result<String, String>>>,
    }
    #[async_trait]
    impl Reasoner for FakeReasoner {
        async fn call(
            &self,
            _opts: &ReasonerOpts,
            _user: &str,
        ) -> anyhow::Result<String> {
            let mut r = self.replies.lock().unwrap();
            match r.remove(0) {
                Ok(s) => Ok(s),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    #[tokio::test]
    async fn extractor_parses_clean_json() {
        let fake = FakeReasoner {
            replies: Mutex::new(vec![Ok(
                "{\"company\":\"Acme\",\"title\":\"CTO\",\"phones\":[],\"confidence\":{\"company\":0.9,\"title\":0.9}}"
                    .into(),
            )]),
        };
        let ex = SignatureExtractor::new(&fake);
        let f = ex.extract("Jane Doe\nCTO at Acme").await.unwrap();
        assert_eq!(f.company.as_deref(), Some("Acme"));
        assert_eq!(f.title.as_deref(), Some("CTO"));
    }

    #[tokio::test]
    async fn extractor_retries_then_regex_fallback() {
        // First reply is junk prose (no JSON); second also junk → fall back
        // to regex which still pulls the phone literally in the block.
        let fake = FakeReasoner {
            replies: Mutex::new(vec![
                Ok("I cannot help with that.".into()),
                Ok("Still no JSON, sorry.".into()),
            ]),
        };
        let ex = SignatureExtractor::new(&fake);
        let f = ex
            .extract("Jane Doe\nCTO\n+1 (415) 555-0100")
            .await
            .unwrap();
        assert_eq!(f.phones, vec!["+14155550100"]);
    }

    #[test]
    fn is_human_accepts_plain_person() {
        // pii-ok — synthetic test fixtures, not real personal data.
        // Plain personal mailbox at a generic domain, no automation signals.
        assert!(is_human_sender("alice@example.com", "Hey, want to grab coffee?")); // pii-ok
        assert!(is_human_sender(
            "\"Bob Smith\" <bob.smith@startup.io>", // pii-ok
            "thanks for the intro",
        ));
        assert!(is_human_sender("Carol <carol@acme.co>", "")); // pii-ok
    }

    #[test]
    fn is_human_rejects_noreply_local_parts() {
        // pii-ok — synthetic test fixtures verifying local-part matcher.
        for from in [
            "no-reply@stripe.com",    // pii-ok
            "noreply@github.com",     // pii-ok
            "donotreply@calendly.com",// pii-ok
            "notifications@linkedin.com", // pii-ok
            "alerts@aws.com",         // pii-ok
            "newsletter@tldr.tech",   // pii-ok
            "marketing@brand.co",     // pii-ok
            "deals@vendor.io",        // pii-ok
            "updates@app.dev",        // pii-ok
            "billing@saas.com",       // pii-ok
            "receipts@uber.com",      // pii-ok
            "team@somecompany.co",    // pii-ok
            "hello@fuggit.dev",       // pii-ok
            "support+sub@github.com", // pii-ok
        ] {
            assert!(!is_human_sender(from, ""), "{from} should not be human");
        }
    }

    #[test]
    fn is_human_rejects_esp_domains() {
        // pii-ok — synthetic ESP-domain fixtures verifying domain matcher.
        // Even with an innocuous local part, the domain alone classifies them as non-human.
        for from in [
            "lenny@substack.com",      // pii-ok
            "writer@beehiiv.com",      // pii-ok
            "alice@buttondown.email",  // pii-ok
            "bounce@mailgun.example.net", // pii-ok
            "x@something.amazonses.com",  // pii-ok
            "person@linkedin.com",     // pii-ok
            "user@coderabbitai.com",   // pii-ok
        ] {
            assert!(!is_human_sender(from, ""), "{from} should not be human");
        }
    }

    #[test]
    fn is_human_rejects_body_markers() {
        // pii-ok — synthetic test fixture for body-marker heuristic.
        // Body carrying bulk-mail boilerplate forces a non-human verdict
        // even if the local part looks innocent.
        let from = "alex@some-random-domain.com"; // pii-ok
        assert!(!is_human_sender(
            from,
            "Hi! List-Unsubscribe: <mailto:u@x.com>\nDeal of the day...", // pii-ok
        ));
        assert!(!is_human_sender(
            from,
            "This is an automated message. Please do not reply to this email.",
        ));
        assert!(!is_human_sender(
            from,
            "View this email in your browser. Manage your subscription here.",
        ));
    }

    #[test]
    fn is_human_rejects_garbage_input() {
        assert!(!is_human_sender("", ""));
        assert!(!is_human_sender("not-an-email", "hi"));
    }

    #[test]
    fn is_human_default_allows_unknown_normal_address() {
        // pii-ok — synthetic test fixture (jane.doe@employer.com is RFC-2606-ish).
        // A perfectly ordinary first.last@employer.com with normal body — // pii-ok
        // we default-allow rather than block.
        assert!(is_human_sender(
            "jane.doe@employer.com", // pii-ok
            "Following up on yesterday's chat — Friday at 2 still works.",
        ));
    }

    #[test]
    fn event_blast_matches_known_domains() {
        // pii-ok — synthetic test fixtures (event-platform senders).
        // (1) Domain category: Partiful, Luma subdomain, Meetup.
        assert!(is_event_blast(
            "\"Partiful\" <invites@partiful-mail.com>", // pii-ok
            "You're Invited: NYC Tech Drinks",
            "Body text here",
        ));
        assert!(is_event_blast(
            "noreply@send.luma-mail.com", // pii-ok
            "Confirming your registration",
            "",
        ));
        assert!(is_event_blast(
            "Meetup <info@email.meetup.com>", // pii-ok
            "Reminder: meetup tomorrow",
            "",
        ));
    }

    #[test]
    fn event_blast_matches_subject_markers() {
        // pii-ok — synthetic subject-marker fixtures.
        // (2) Subject category: Registration Confirmed, RSVP, "see you Wed".
        assert!(is_event_blast(
            "Eventbrite <noreply@somewhere.example>", // pii-ok
            "Registration Confirmed for AI Demo Night",
            "irrelevant body",
        ));
        assert!(is_event_blast(
            "host@randomdomain.example", // pii-ok
            "RSVP requested: brunch",
            "",
        ));
        assert!(is_event_blast(
            "organizer@randomdomain.example", // pii-ok
            "See you Wed at the loft!",
            "",
        ));
        // Variant: "see you next Thursday" via the modifier branch.
        assert!(is_event_blast(
            "organizer@randomdomain.example", // pii-ok
            "See you next Thursday",
            "",
        ));
    }

    #[test]
    fn event_blast_matches_body_markers() {
        // pii-ok — synthetic body-marker fixtures.
        // (3) Body category: calendar artifacts.
        assert!(is_event_blast(
            "host@unknown.example", // pii-ok
            "quick note",
            "Looking forward to seeing you. Add to Calendar: link",
        ));
        assert!(is_event_blast(
            "host@unknown.example", // pii-ok
            "details",
            "ical://example.com/event.ics",
        ));
        assert!(is_event_blast(
            "host@unknown.example", // pii-ok
            "fyi",
            "RSVP via https://www.google.com/calendar/event?eid=abc",
        ));
    }

    #[test]
    fn event_blast_matches_calendar_noreply_local_part() {
        // pii-ok — synthetic calendar-noreply fixture.
        // The `noreply@calendar.<corp>` rule catches Calendly /
        // Google-Calendar-style bounce-back senders.
        assert!(is_event_blast(
            "noreply@calendar.google.com", // pii-ok
            "Invitation: Coffee chat",
            "",
        ));
    }

    #[test]
    fn event_blast_matches_invitation_unknown_sender_subject() {
        // pii-ok — synthetic Google-Calendar "unknown sender" subject.
        assert!(is_event_blast(
            "no-reply@somecorp.example", // pii-ok
            "Invitation from an unknown sender: project sync",
            "",
        ));
    }

    #[test]
    fn event_blast_passes_through_real_personal_mail() {
        // pii-ok — synthetic personal-mail negatives (no event signals).
        // (4) Negative controls: ordinary personal correspondence MUST NOT
        // match — these would otherwise lose a draft.
        assert!(!is_event_blast(
            "alice@example.com",                                 // pii-ok
            "Re: Coffee tomorrow?",                              // pii-ok
            "Sounds good — see you at the usual spot at 9am.",   // pii-ok
        ));
        assert!(!is_event_blast(
            "bob.smith@startup.io",                              // pii-ok
            "Quick question on the deploy",                      // pii-ok
            "Hey — did the migration land yet? Need to plan around it.", // pii-ok
        ));
    }

    /// #834 — the exact shape that slipped through: a forwarded Microsoft
    /// Teams invite from a human sender that triage marked `reply`.
    #[test]
    fn meeting_invite_matches_forwarded_teams_invite() {
        // pii-ok — synthetic forwarded-Teams-invite fixture.
        let body = "\
________________________________________
Microsoft Teams meeting
Join on your computer, mobile app or room device
Join the meeting now
Meeting ID: 123 456 789
When: Wednesday, September 2 1:00 PM-2:00 PM
Where: Microsoft Teams
";
        assert!(is_meeting_invite("FW: Q3 Sync", body, &[]));
    }

    #[test]
    fn meeting_invite_matches_calendar_attachment_labels() {
        // pii-ok — synthetic attachment-label fixtures.
        assert!(is_meeting_invite(
            "Q3 Sync",
            "See attached.",
            &["invite.ics (text/calendar)".to_string()],
        ));
        assert!(is_meeting_invite(
            "Q3 Sync",
            "See attached.",
            &["Meeting.ICS".to_string()],
        ));
    }

    #[test]
    fn meeting_invite_matches_invitation_subjects() {
        // pii-ok — synthetic calendar-client subject fixtures.
        assert!(is_meeting_invite("Invitation: standup @ Tue", "", &[]));
        assert!(is_meeting_invite(
            "Fwd: Fw: Updated invitation: Perry sync",
            "",
            &[],
        ));
        assert!(is_meeting_invite("Canceled event: design review", "", &[]));
        assert!(is_meeting_invite("Declined: coffee chat", "", &[]));
    }

    /// A forward strips the `.ics` part, and only Teams bodies carry the
    /// "Microsoft Teams meeting" literal — so for every other client the
    /// join link and the organizer/attendee header block are the only
    /// signals left. Each is asserted on its own here.
    #[test]
    fn meeting_invite_matches_generic_join_links_and_headers() {
        // pii-ok — synthetic forwarded-invite fixtures.
        assert!(is_meeting_invite(
            "FW: Project sync",
            "Join Zoom Meeting\nhttps://zoom.us/j/1234567890?pwd=abcdef\n",
            &[],
        ));
        assert!(is_meeting_invite(
            "Fwd: Project sync",
            "Join with Google Meet\nhttps://meet.google.com/abc-defg-hij\n",
            &[],
        ));
        // Header block alone — no join link, and an online meeting so the
        // Outlook forward carries no `Where:` line.
        assert!(is_meeting_invite(
            "FW: Project sync",
            "When: Wednesday, September 2 1:00 PM\n\
             Organizer: Alice Example <alice@example.com>\n\
             Required Attendees: bob@example.com\n",
            &[],
        ));
    }

    #[test]
    fn meeting_invite_matches_google_calendar_body() {
        // pii-ok — synthetic Google Calendar boilerplate fixture.
        assert!(is_meeting_invite(
            "Re: sync",
            "Invitation from Google Calendar\n\nYou are receiving this email...",
            &[],
        ));
    }

    #[test]
    fn meeting_invite_passes_through_ordinary_mail() {
        // pii-ok — synthetic negatives. Each of these would lose a draft if
        // the detection were loosened.
        // Teams named in prose without any join markers.
        assert!(!is_meeting_invite(
            "Re: next steps",
            "Let's do a Teams meeting sometime next week — does Thursday work?",
            &[],
        ));
        // Mid-sentence "when:" with no line-anchored invite headers.
        assert!(!is_meeting_invite(
            "Re: timeline",
            "Still unclear when: the vendor keeps moving the date.",
            &[],
        ));
        // A single stray invite header is not a header block.
        assert!(!is_meeting_invite(
            "Re: lunch",
            "Where: the usual spot. Works for you?",
            &[],
        ));
        // A standing room link pasted in prose, with no `Join ...` line.
        assert!(!is_meeting_invite(
            "Re: thursday",
            "Easier on my room if you'd rather: https://zoom.us/j/1234567890",
            &[],
        ));
        // Plain reply-worthy mail with an ordinary attachment.
        assert!(!is_meeting_invite(
            "Quick question on the deploy",
            "Did the migration land yet? Need to plan around it.",
            &["proposal.pdf (application/pdf)".to_string()],
        ));
        // `.ics` must read as a filename extension, not any substring.
        assert!(!is_meeting_invite(
            "Numbers for Q3",
            "Export attached.",
            &["metrics.icsv (text/csv)".to_string()],
        ));
        assert!(!is_meeting_invite("", "", &[]));
    }

    #[tokio::test]
    async fn extractor_errors_when_nothing_extractable() {
        let fake = FakeReasoner {
            replies: Mutex::new(vec![
                Ok("nope".into()),
                Ok("still nope".into()),
            ]),
        };
        let ex = SignatureExtractor::new(&fake);
        // Block with no phone/url and unparseable LLM → SigError::Parse.
        assert!(matches!(
            ex.extract("Jane Doe\nSenior Person").await,
            Err(SigError::Parse)
        ));
    }
}

#[cfg(test)]
mod bulk_sender_449 {
    use super::is_human_sender;

    /// Shapes that actually reached the live approval queue as "reply-worthy"
    /// drafts (#451). Addresses are synthetic stand-ins for the real senders;
    /// what matters is the shape each one probes.
    #[test]
    fn bulk_senders_from_the_live_queue_are_not_human() {
        for from in [
            // bulk local-part on a sending subdomain
            "Brand <marketing@engage.examplebrand.com>", // pii-ok: synthetic test fixture
            // `offers@` — the local-part that proved the list was too short
            "\"Example News\" <offers@examplenews.com>", // pii-ok: synthetic test fixture
            // single-letter `e.` sending subdomain
            "EXAMPLE RACK <rack@e.examplerack.com>", // pii-ok: synthetic test fixture
            // known ESP domain
            "A Writer <writer-c0ed52@mail.beehiiv.com>", // pii-ok: synthetic test fixture
            // `newsletter@` local-part
            "Example Digest <newsletter@digest.example.com>", // pii-ok: synthetic test fixture
            // `events@` local-part
            "Example Events <events@example.io>", // pii-ok: synthetic test fixture
        ] {
            assert!(
                !is_human_sender(from, ""),
                "expected bulk sender to be filtered: {from}"
            );
        }
    }

    /// The filter is default-allow by design — tightening it must not start
    /// swallowing the real people the whole product exists to reply to. These
    /// mirror the senders whose threads went undrafted in #450.
    #[test]
    fn real_humans_are_still_human() {
        for from in [
            "Dana Rivera <dana@example-labs.ai>", // pii-ok: synthetic test fixture
            "Sam Okafor <sam@example-labs.ai>",   // pii-ok: synthetic test fixture
            "Alex Chen <alex@examplesoft.net>",   // pii-ok: synthetic test fixture
            "A Person <a.person@gmail.com>",      // pii-ok: synthetic test fixture
            "Someone <someone@hey.com>",          // pii-ok: synthetic test fixture
        ] {
            assert!(
                is_human_sender(from, ""),
                "expected human sender to pass: {from}"
            );
        }
    }

    /// `gmail.com` must never match the `mail` bulk-subdomain label: `gmail` is
    /// one apex label, not a leading sending subdomain. Only the leading labels
    /// of a 3+-label domain are considered.
    #[test]
    fn apex_domains_are_not_mistaken_for_sending_subdomains() {
        assert!(is_human_sender("A <a@gmail.com>", "")); // pii-ok: synthetic test fixture
        assert!(!is_human_sender("C <c@news.example.com>", "")); // pii-ok: synthetic
        assert!(!is_human_sender("D <d@e.retailer.com>", "")); // pii-ok: synthetic
    }

    /// #253: the real address is the LAST `<addr>` pair. A bracketed fragment
    /// hidden in the display name must not shadow it.
    #[test]
    fn extract_bare_takes_the_last_bracketed_address() {
        use super::extract_bare;
        assert_eq!(extract_bare("Foo <foo@bar.com>"), "foo@bar.com"); // pii-ok: synthetic
        assert_eq!(
            extract_bare("\"<a@gmail.com>\" <noreply@stripe.com>"), // pii-ok: synthetic
            "noreply@stripe.com" // pii-ok: synthetic
        );
        assert_eq!(extract_bare("plain@nobrackets.com"), "plain@nobrackets.com"); // pii-ok: synthetic
    }

    /// #253: a bulk sender that hides a human-looking address in its display
    /// name must still be filtered — the true addr-spec (last `<...>`) is
    /// `noreply@`, not the spoofed gmail fragment.
    #[test]
    fn is_human_rejects_embedded_bracket_spoof() {
        assert!(
            !is_human_sender("\"<a@gmail.com>\" <noreply@stripe.com>", ""), // pii-ok: synthetic
            "embedded-bracket spoof must not bypass the human-sender gate"
        );
        // Regression: a genuine human is unaffected by the rfind change.
        assert!(is_human_sender("A Person <a.person@gmail.com>", "")); // pii-ok: synthetic
    }
}
