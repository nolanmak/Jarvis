//! Tone-mirroring v1 (#73): cleaning + filter rules for sent-mail bodies.
//!
//! The Composio sent-history backfill pipes raw bodies through here before
//! handing them to `Store::insert_tone_example`. The same rules also gate
//! user-edit ingestion so the summarizer corpus stays clean.

use augmentagent_store::Email;

use crate::gmail::extract_bare_email;

/// Strip quoted-reply blocks, sig blocks, and `> `-prefixed quote lines from
/// a raw sent-mail body. Idempotent — running it twice on the same input
/// returns the same result.
///
/// Order matters: first kill the "On <date>, X wrote:" reply preamble, then
/// drop any leftover `> `-prefixed lines anywhere in the body, then trim
/// trailing signatures.
pub fn clean_sent_body(raw: &str) -> String {
    // Normalize CRLF → LF up-front so all subsequent index math is on a
    // single line-ending. Gmail and Composio both emit either depending on
    // the source MIME part.
    let normalized = raw.replace("\r\n", "\n");

    // 1. Strip Gmail's quoted-reply preamble. Pattern is: a line starting
    //    with "On ", followed eventually by " wrote:" — anything from that
    //    "On " through the end of the body is the reply chain. We match on
    //    "\nOn " specifically so we don't truncate a body that *starts* with
    //    "On Thursday I shipped...". We then look for " wrote:" within the
    //    same tail to confirm it's a Gmail-style preamble.
    let mut s = if let Some(on_pos) = normalized.find("\nOn ") {
        if normalized[on_pos..].contains(" wrote:") {
            normalized[..on_pos].to_string()
        } else {
            normalized
        }
    } else {
        normalized
    };

    // 2. Strip "> "-prefixed quote lines anywhere in the (possibly already
    //    truncated) body. Keep blank lines so paragraph structure isn't lost.
    s = s
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n");

    // 3. Strip RFC 3676 signature delimiter ("\n-- \n").
    if let Some(pos) = s.find("\n-- \n") {
        s.truncate(pos);
    }
    // 4. Strip the canonical mobile-mail trailer.
    if let Some(pos) = s.find("\nSent from my ") {
        s.truncate(pos);
    }

    s.trim().to_string()
}

/// Filter result from `should_keep_for_tone`. Plain bools force the caller
/// to choose a label every time, which surfaces the keep-vs-drop reasoning
/// in code review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneFilter {
    Keep,
    DropTooShort,
    DropTooLong,
    DropSelfRecipient,
    DropNoReplyRecipient,
    DropEmpty,
}

/// Minimum cleaned-body characters worth feeding the summarizer. One-liners
/// like "thanks!" don't teach voice.
const MIN_BODY_CHARS: usize = 40;

/// Maximum cleaned-body characters. Anything larger is almost always a
/// forwarded thread that escaped the quote stripper, or pasted documentation.
const MAX_BODY_CHARS: usize = 4_000;

/// Patterns that mark a recipient as automated. Compared against the local
/// part (before `@`) lowercased. Anything matching is a no-reply mailbox we
/// should never treat as someone the user "writes to".
const NOREPLY_LOCAL_PATTERNS: &[&str] = &[
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
];

/// Decide whether a cleaned sent-mail body deserves a row in `tone_examples`.
/// `self_addrs` is the set of email addresses owned by the user (typically
/// the active gmail accounts) — replies to those are notes-to-self that
/// would distort the voice profile.
pub fn should_keep_for_tone(
    cleaned_body: &str,
    recipient_bare: &str,
    self_addrs: &[String],
) -> ToneFilter {
    let trimmed = cleaned_body.trim();
    if trimmed.is_empty() {
        return ToneFilter::DropEmpty;
    }
    let chars = trimmed.chars().count();
    if chars < MIN_BODY_CHARS {
        return ToneFilter::DropTooShort;
    }
    if chars > MAX_BODY_CHARS {
        return ToneFilter::DropTooLong;
    }
    let recipient_lower = recipient_bare.trim().to_ascii_lowercase();
    if recipient_lower.is_empty() {
        return ToneFilter::DropEmpty;
    }
    if self_addrs.iter().any(|a| a.to_ascii_lowercase() == recipient_lower) {
        return ToneFilter::DropSelfRecipient;
    }
    if let Some((local, _domain)) = recipient_lower.split_once('@') {
        for pattern in NOREPLY_LOCAL_PATTERNS {
            if local.contains(pattern) {
                return ToneFilter::DropNoReplyRecipient;
            }
        }
    } else {
        // No @ at all → not a real address.
        return ToneFilter::DropEmpty;
    }
    ToneFilter::Keep
}

/// Convenience: pull the (bare, lowercased) recipient out of an Email's
/// `from` field. The backfill query is `in:sent`, so on those rows the
/// `from` slot is the *recipient* of what we sent — our schema just hasn't
/// renamed the field. Caller passes this into `should_keep_for_tone` and
/// `Store::insert_tone_example`.
pub fn recipient_from_sent(email: &Email) -> (String, String) {
    let bare = extract_bare_email(&email.from).to_ascii_lowercase();
    let domain = bare
        .split_once('@')
        .map(|(_, d)| d.to_string())
        .unwrap_or_default();
    (bare, domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_gmail_reply_preamble() {
        let raw = "Sounds good — let's ship Friday.\n\nOn Thu, Apr 18, 2026 at 3:14 PM, Alex <alex@x.io> wrote:\n> any update?\n> still on track?\n";
        assert_eq!(clean_sent_body(raw), "Sounds good — let's ship Friday.");
    }

    #[test]
    fn clean_handles_crlf_line_endings() {
        let raw = "Hey there\r\n\r\nQuick one — call later?\r\n\r\nOn Mon, Apr 1, 2026 at 9:00 AM, X wrote:\r\n> ping\r\n";
        let out = clean_sent_body(raw);
        assert_eq!(out, "Hey there\n\nQuick one — call later?");
    }

    #[test]
    fn clean_strips_inline_quoted_lines() {
        let raw = "I disagree with this:\n> the deploy is blocking\nIt isn't — staging is up.\n";
        assert_eq!(
            clean_sent_body(raw),
            "I disagree with this:\nIt isn't — staging is up."
        );
    }

    #[test]
    fn clean_strips_rfc3676_signature() {
        let raw = "Body proper.\n\nThanks,\nNolan\n\n-- \nNolan Makatche\n+1-555-0100\n";
        assert_eq!(clean_sent_body(raw), "Body proper.\n\nThanks,\nNolan");
    }

    #[test]
    fn clean_strips_sent_from_my_phone_trailer() {
        let raw = "Quick yes from my phone.\n\nSent from my iPhone\n";
        assert_eq!(clean_sent_body(raw), "Quick yes from my phone.");
    }

    #[test]
    fn clean_preserves_body_starting_with_on() {
        // A body that *starts* with "On Thursday..." must not be truncated.
        // We only match "\nOn " (newline + "On "), so a body whose very
        // first chars are "On " is fine.
        let raw = "On Thursday I shipped the change. Confirming Friday is still good.\n";
        let out = clean_sent_body(raw);
        assert_eq!(
            out,
            "On Thursday I shipped the change. Confirming Friday is still good."
        );
    }

    #[test]
    fn clean_leaves_on_without_wrote_alone() {
        // "\nOn " followed by something that ISN'T " wrote:" must NOT be
        // treated as a Gmail preamble. (e.g. a sentence break.)
        let raw = "First line.\nOn the topic of pricing, here's my take:\nBlah.\n";
        let out = clean_sent_body(raw);
        assert_eq!(
            out,
            "First line.\nOn the topic of pricing, here's my take:\nBlah."
        );
    }

    #[test]
    fn clean_returns_empty_for_pure_quoted_chain() {
        let raw = "> quoted\n> more quoted\n";
        assert_eq!(clean_sent_body(raw), "");
    }

    #[test]
    fn clean_idempotent() {
        let raw = "Body.\n\nOn Thu, Apr 18, 2026 at 3:14 PM, Alex wrote:\n> stuff\n";
        let once = clean_sent_body(raw);
        let twice = clean_sent_body(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn clean_handles_empty_input() {
        assert_eq!(clean_sent_body(""), "");
        assert_eq!(clean_sent_body("   \n\n   "), "");
    }

    #[test]
    fn filter_drops_short_body() {
        assert_eq!(
            should_keep_for_tone("thanks!", "alex@x.com", &[]),
            ToneFilter::DropTooShort
        );
    }

    #[test]
    fn filter_drops_oversize_body() {
        let big = "x".repeat(4_500);
        assert_eq!(
            should_keep_for_tone(&big, "alex@x.com", &[]),
            ToneFilter::DropTooLong
        );
    }

    #[test]
    fn filter_drops_self_recipient() {
        let body = "This is a long-enough body to pass the chars filter, easily.";
        assert_eq!(
            should_keep_for_tone(body, "Me@Example.com", &["me@example.com".into()]),
            ToneFilter::DropSelfRecipient
        );
    }

    #[test]
    fn filter_drops_noreply_pattern() {
        let body = "This is a long-enough body to pass the chars filter, easily.";
        for to in [
            "no-reply@stripe.com",
            "noreply@github.com",
            "notifications@linkedin.com",
            "alerts@aws.com",
            "donotreply@calendly.com",
        ] {
            assert_eq!(
                should_keep_for_tone(body, to, &[]),
                ToneFilter::DropNoReplyRecipient,
                "expected {to} dropped as no-reply"
            );
        }
    }

    #[test]
    fn filter_keeps_normal_body() {
        let body = "Sounds great — Friday at 2pm works for me. I'll send a calendar invite.";
        assert_eq!(
            should_keep_for_tone(body, "alex@startup.io", &["me@example.com".into()]),
            ToneFilter::Keep
        );
    }

    #[test]
    fn filter_drops_bare_recipient_without_at() {
        let body = "This body is plenty long enough to pass the minimum chars check.";
        assert_eq!(
            should_keep_for_tone(body, "garbage", &[]),
            ToneFilter::DropEmpty
        );
    }
}
