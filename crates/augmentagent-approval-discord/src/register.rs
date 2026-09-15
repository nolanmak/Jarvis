//! Register (casing) receipt on outbound drafts (#994).
//!
//! `schema/wiki-ask.md` makes the owner's chameleon rule auditable: every
//! outbound draft is preceded by a one-line receipt naming the register the
//! recipient uses (`register: standard (she capitalizes), mirroring`), and
//! the draft sits in a fenced block under it, so the delivery layer knows
//! where the draft starts and ends.
//!
//! The receipt is model-authored, so by itself it is a promise. This module
//! is the deterministic half: every paragraph of the draft is compared with
//! its receipt and a contradiction becomes a note the caller posts as a
//! visible `⚠️` line — the same channel a refused `ATTACH:` marker uses.
//! A draft with no receipt above it is reported as unchecked, where "draft"
//! is judged from the text alone, not from the drafter's compliance: a
//! fenced block (by protocol a fence in a reply IS a draft), or a paragraph
//! that opens like a message (`hey casey,`), which is how the #994 draft
//! looks when the drafter skips the protocol entirely. What a text-only
//! check cannot see is a receiptless, unfenced draft with no greeting; that
//! is prose to this layer, and the reason the protocol makes the fence
//! mandatory rather than the reason to trust it.
//!
//! Where drafts leave the system, and where this audit runs (verified call
//! sites, not the protocol's wish list):
//! - Discord replies (the owner hand-pastes texts, DMs and social replies
//!   from them): the two posters of wiki-ask answers, `event_handler.rs`
//!   and `wiki ask --post` in `main.rs`, both via
//!   `attachments::prepare_answer_delivery`; `/loop` results post via
//!   `LoopPoster` and audit in `loops.rs`. The WhatsApp control surface also
//!   calls `QueryHandler::answer`, but the CLI never constructs it (#74).
//! - Email bodies, the one recipient-facing payload a wiki-ask tool sends:
//!   `gmail compose|update-draft|send-now` (`main.rs`) require the receipt
//!   as the body's first line when run from the drafter's env, refuse a
//!   contradiction before any Gmail write, then strip the receipt.
//!   `calendar create-event` and `aa-gh issue` do not address a recipient.
//!
//! A paragraph is judged all-or-nothing on its sentence starts; mixed casing
//! is either a correct draft with a lowercase URL/handle at a sentence start
//! or a judgment call the receipt already makes visible to the owner.

use std::fmt;

/// Receipt prefix, matched at line start after trimming and ASCII
/// case-insensitively, so a `Register:` in a sentence-cased reply counts.
pub const REGISTER_RECEIPT_PREFIX: &str = "register:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Register {
    Lowercase,
    Standard,
}

impl fmt::Display for Register {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Register::Lowercase => "lowercase",
            Register::Standard => "standard",
        })
    }
}

/// Parse one line as a receipt. `unknown` resolves to the default it names
/// (`defaulting to lowercase`); an `unknown` naming none checks nothing.
fn parse_receipt(line: &str) -> Option<Option<Register>> {
    let trimmed = line.trim();
    let head = trimmed.get(..REGISTER_RECEIPT_PREFIX.len())?;
    if !head.eq_ignore_ascii_case(REGISTER_RECEIPT_PREFIX) {
        return None;
    }
    let rest = trimmed[REGISTER_RECEIPT_PREFIX.len()..].trim_start();
    let word: String = rest
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    match word.as_str() {
        "lowercase" => Some(Some(Register::Lowercase)),
        "standard" => Some(Some(Register::Standard)),
        "unknown" => {
            let tail = rest[word.len()..].to_ascii_lowercase();
            let default = tail
                .split_once("defaulting to")
                .map(|(_, after)| after.trim_start())
                .and_then(|after| {
                    if after.starts_with("lowercase") {
                        Some(Register::Lowercase)
                    } else if after.starts_with("standard") {
                        Some(Register::Standard)
                    } else {
                        None
                    }
                });
            Some(default)
        }
        _ => None,
    }
}

/// True when `line` is a register receipt — for a send path that gates a
/// payload on the receipt heading it and drops that owner-facing line.
pub fn is_register_receipt(line: &str) -> bool {
    parse_receipt(line).is_some()
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Paragraph break inside a draft: a blank line, or a bare `>` in a
/// blockquoted one.
fn is_break(line: &str) -> bool {
    matches!(line.trim(), "" | ">")
}

/// The draft a receipt vouches for, as paragraphs, plus the line to resume
/// scanning at. A fenced block under the receipt is the draft in full (the
/// protocol's form); a run of `>` blockquote lines is bounded the same way.
/// Anything else is read through to the next receipt or the end of the
/// reply, so an unfenced multi-paragraph draft is still inspected whole — at
/// the cost of also reading any commentary the drafter put after it, which
/// is what the fence exists to separate.
fn draft_paragraphs<'a>(lines: &[&'a str], receipt_idx: usize) -> (Vec<Vec<&'a str>>, usize) {
    let mut i = receipt_idx + 1;
    while i < lines.len() && is_break(lines[i]) {
        i += 1;
    }
    let fenced = i < lines.len() && is_fence(lines[i]);
    let quoted = !fenced && i < lines.len() && lines[i].trim_start().starts_with('>');
    i += usize::from(fenced);
    let (mut paragraphs, mut current) = (Vec::new(), Vec::new());
    while i < lines.len() {
        let line = lines[i];
        let ends = if fenced {
            is_fence(line)
        } else if quoted {
            !line.trim_start().starts_with('>')
        } else {
            parse_receipt(line).is_some()
        };
        if ends {
            i += usize::from(fenced);
            break;
        }
        i += 1;
        if is_break(line) {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else if !is_fence(line) {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    (paragraphs, i)
}

/// Casing at each sentence start of the paragraph: the first word of every
/// line and every word after a `.`/`!`/`?`. Only sentence-shaped lines (three
/// or more words) are evidence — a greeting or a sign-off is cased either
/// way — and so is a list item's first word, so a list marker demotes it.
/// URL/handle/abbreviation-shaped words (`github.com/x`, `@sam`, `e.g.`)
/// are skipped because their casing says nothing about register.
fn sentence_starts(paragraph: &[&str]) -> (usize, usize) {
    let (mut upper, mut lower) = (0, 0);
    for line in paragraph {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.iter().filter(|w| w.chars().any(char::is_alphabetic)).count() < 3 {
            continue;
        }
        let mut at_start = true;
        for (n, word) in words.iter().enumerate() {
            let Some(first) = word.chars().find(|c| c.is_alphabetic()) else {
                // `-`, `1.`, `•` open a list item; a blockquote `>` does not.
                at_start &= n > 0 || *word == ">";
                continue;
            };
            // Trimming the edges leaves only interior dots, so `e.g.` and
            // `github.com` are opaque while a sentence-final `tonight.` is not.
            let opaque = word
                .trim_matches(|c: char| !c.is_alphanumeric())
                .contains(['/', '@', '.']);
            if at_start && !opaque {
                if first.is_uppercase() {
                    upper += 1;
                } else {
                    lower += 1;
                }
            }
            at_start = word
                .trim_end_matches(['"', '\'', ')', '*', '_'])
                .ends_with(['.', '!', '?'])
                && !opaque;
        }
    }
    (upper, lower)
}

/// The first paragraph whose casing contradicts `declared`, as a note.
fn contradiction(declared: Register, paragraphs: &[Vec<&str>]) -> Option<String> {
    paragraphs.iter().enumerate().find_map(|(n, p)| {
        let observed = match sentence_starts(p) {
            (0, k) if k > 0 => Register::Lowercase,
            (k, 0) if k > 0 => Register::Standard,
            _ => return None,
        };
        (observed != declared).then(|| {
            let reads = match observed {
                Register::Lowercase => "all-lowercase",
                Register::Standard => "capitalized",
            };
            format!(
                "register mismatch: the receipt says {declared} but paragraph {} of the \
                 draft under it is {reads} (#994) — ask for a recase",
                n + 1
            )
        })
    })
}

/// A line that opens like a message to someone — `hey casey,` / `hi,` /
/// `hello sam!` — outside any receipted draft. The punctuation after the
/// greeting or the name is required so a reply that merely begins "hi there
/// is no thread on file" is not a draft.
fn opens_like_a_message(line: &str) -> bool {
    const GREETINGS: [&str; 6] = ["hey", "hi", "hello", "dear", "yo", "hiya"];
    let mut words = line.split_whitespace();
    let Some(first) = words.next() else { return false };
    let greeting = first.trim_end_matches(',');
    GREETINGS.contains(&greeting.to_ascii_lowercase().as_str())
        && (greeting.len() < first.len()
            || words.next().is_some_and(|name| name.ends_with([',', '!', '.'])))
}

const UNCHECKED_NOTE: &str = "draft with no `register:` receipt above it — its casing was not \
                              checked against the recipient (#994)";

fn scan(text: &str, discord: bool) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut notes = Vec::new();
    let mut unchecked = false;
    let mut i = 0;
    while i < lines.len() {
        if let Some(declared) = parse_receipt(lines[i]) {
            let (paragraphs, next) = draft_paragraphs(&lines, i);
            if let Some(declared) = declared {
                notes.extend(contradiction(declared, &paragraphs));
            }
            i = next;
        } else if is_fence(lines[i]) {
            unchecked |= discord;
            i += 1;
            while i < lines.len() && !is_fence(lines[i]) {
                i += 1;
            }
            i += 1;
        } else {
            unchecked |= discord && opens_like_a_message(lines[i]);
            i += 1;
        }
    }
    if unchecked {
        notes.push(UNCHECKED_NOTE.to_string());
    }
    notes
}

/// Notes for a Discord reply: each receipt is checked against every
/// paragraph of the draft under it, and a draft with no receipt above it —
/// a bare fenced block, or a paragraph that opens like a message — is
/// reported as unchecked (once per reply), so the #994 draft surfaces
/// whether the drafter got the receipt wrong or skipped the protocol.
pub fn audit_discord_reply(text: &str) -> Vec<String> {
    scan(text, true)
}

/// Receipt-versus-draft notes only, for a payload the drafter heads with its
/// receipt (an email body), where a fence is content rather than a draft.
pub fn audit_register_receipts(text: &str) -> Vec<String> {
    scan(text, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #994 verbatim: the recipient's thread reads "Hi, I wanted to check in
    /// on the proposal." (sentence case) and the draft came out all-lowercase
    /// — the exact draft the owner had to catch by eye. Flagged as a mismatch
    /// when the drafter classified correctly and still ignored itself (fenced
    /// or not), and as unchecked when it skipped the protocol and returned
    /// the bare draft, which is what the reported turn actually looked like.
    #[test]
    fn issue_994_lowercase_draft_for_a_capitalizing_recipient_is_flagged() {
        let draft = "hey casey, thanks for checking in. i'll have the proposal \
                     over tonight, let me know if thursday still works.";
        for reply in [
            format!("register: standard (she capitalizes), mirroring\n{draft}\n\nFiled the thread to people/casey.md."),
            format!("register: standard (she capitalizes), mirroring\n```\n{draft}\n```\nFiled the thread to people/casey.md."),
        ] {
            let notes = audit_discord_reply(&reply);
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains("receipt says standard"), "{}", notes[0]);
            assert!(notes[0].contains("paragraph 1 of the draft under it is all-lowercase"), "{}", notes[0]);
        }
        let bare = audit_discord_reply(draft);
        assert_eq!(bare.len(), 1, "{bare:?}");
        assert!(bare[0].contains("no `register:` receipt"), "{}", bare[0]);
    }

    /// Review finding: a wrong-register paragraph after a matching one must
    /// not hide behind the first paragraph — every paragraph is audited, and
    /// a fence bounds the draft so commentary after it is not.
    #[test]
    fn every_paragraph_of_the_draft_is_audited() {
        let unfenced = "register: lowercase (he types all-lowercase), mirroring\n\
                        hey Sam,\n\nThanks for checking in on the proposal.";
        let notes = audit_discord_reply(unfenced);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("paragraph 2 of the draft under it is capitalized"), "{}", notes[0]);
        let fenced = "register: lowercase (he types all-lowercase), mirroring\n```\n\
                      hey sam, running late.\n\nThanks for checking in on the proposal.\n```\n\n\
                      Filed the thread to people/sam.md.";
        assert_eq!(audit_discord_reply(fenced).len(), 1);
        let fenced_ok = "register: lowercase (he types all-lowercase), mirroring\n```\n\
                         hey sam, running late.\n\nthanks for checking in on the proposal.\n```\n\n\
                         Filed the thread to people/sam.md.";
        assert!(audit_discord_reply(fenced_ok).is_empty());
    }

    #[test]
    fn matching_drafts_pass_silently() {
        let standard = "register: standard (she capitalizes), mirroring\n```\n\
                        Hey Casey, thanks for checking in. I'll have the proposal \
                        over tonight.\n\nTalk soon.\n```\nFiled to people/casey.md.";
        assert!(audit_discord_reply(standard).is_empty());
        let quoted = "register: lowercase (he types all-lowercase), mirroring\n\
                      > hey sam, running 10 late. order without me\n>\n> see you there in a bit\n\n\
                      Filed to people/sam.md.";
        assert!(audit_discord_reply(quoted).is_empty());
    }

    #[test]
    fn capitalized_draft_under_lowercase_or_defaulted_receipt_is_flagged() {
        let notes = audit_discord_reply("Register: lowercase (you asked)\n```\nHey Sam, running late. Order without me.\n```");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("receipt says lowercase") && notes[0].contains("capitalized"), "{}", notes[0]);
        let draft = "Hi Alex, quick question about the venue.";
        assert_eq!(audit_discord_reply(&format!("register: unknown, defaulting to lowercase (no samples on file)\n{draft}")).len(), 1);
        assert!(audit_discord_reply(&format!("register: unknown (no samples on file)\n{draft}")).is_empty());
    }

    #[test]
    fn receiptless_drafts_are_reported_once_and_only_on_discord_replies() {
        let fenced = "here is the draft:\n```\nhey casey, thanks for checking in on the proposal.\n```";
        let greeted = "here you go:\n\nHi Alice,\n\nThanks for checking in.\n\nHello again,\njo";
        for reply in [fenced, greeted] {
            let notes = audit_discord_reply(reply);
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains("no `register:` receipt above it"), "{}", notes[0]);
            assert!(audit_register_receipts(reply).is_empty());
        }
    }

    #[test]
    fn non_evidence_and_receiptless_prose_are_left_alone() {
        let standard = "register: standard (he capitalizes), mirroring\n";
        // URL at a sentence start is not evidence; the second sentence is.
        let url_first = format!("{standard}github.com/example/repo is failing on main. Can you take a look?");
        assert!(audit_discord_reply(&url_first).is_empty());
        // Greetings, sign-offs and list items are cased either way.
        let short_and_listed = format!("{standard}hi alice,\n\nHere is the plan for Thursday:\n- send the deck\n- book the room\n\nbest,\njo");
        assert!(audit_discord_reply(&short_and_listed).is_empty());
        // A greeting word without message punctuation is not a draft opener.
        assert!(audit_discord_reply("filed the note.\nhi there is no thread on file for her.").is_empty());
        assert!(audit_discord_reply("register: standard\n\n").is_empty());
        // Prose that merely mentions the word is not a receipt.
        assert!(!is_register_receipt("the register: standard here"));
        assert!(!is_register_receipt("register: closed for the night"));
        assert!(is_register_receipt("  Register: Standard (she capitalizes)"));
    }
}
