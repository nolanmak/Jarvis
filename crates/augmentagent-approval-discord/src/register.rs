//! Register (casing) receipt on outbound drafts (#994).
//!
//! `schema/wiki-ask.md` makes the owner's chameleon rule auditable: every
//! outbound draft is preceded by a one-line receipt naming the register the
//! recipient uses (`register: standard (she capitalizes), mirroring`), and
//! the draft sits in a fenced block under it, so the delivery layer knows
//! where the draft starts and ends.
//!
//! The receipt is model-authored, so by itself it is a promise. This module
//! is the deterministic half: every sentence start of the draft under a
//! receipt is compared with the receipt, and one that contradicts it becomes
//! a note the caller posts as a visible `⚠️` line — the same channel a
//! refused `ATTACH:` marker uses. The receipt line is the whole envelope:
//! text with no receipt is not audited, so a code example in a wiki answer,
//! a quoted inbound email or a loop's status report never draws a note.
//! Guessing at drafts from their shape (a bare fence, a greeting line) was
//! tried and flagged ordinary answers; a drafter that skips the protocol
//! entirely is a prompt failure the owner sees as a draft with no receipt.
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
    let (mut paragraphs, mut current, mut in_code) = (Vec::new(), Vec::new(), false);
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
        } else if is_fence(line) {
            // A code snippet inside an unfenced draft (an email body) is
            // content, not prose.
            in_code = !in_code;
        } else if !in_code {
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
/// URL/handle/abbreviation/brand-shaped words (`github.com/x`, `@sam`,
/// `e.g.`, `iPhone`) are skipped because their casing says nothing about
/// register.
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
                .contains(['/', '@', '.'])
                || (first.is_lowercase() && word.chars().skip(1).any(char::is_uppercase));
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

/// The first paragraph with a sentence start that contradicts `declared`,
/// as a note. One is enough: a `standard` recipient reading a draft that
/// opens `hey Casey, thanks for checking in. I'll send it tonight.` still
/// gets the lowercase opener, so mixed evidence is a violation, not a wash.
fn contradiction(declared: Register, paragraphs: &[Vec<&str>]) -> Option<String> {
    paragraphs.iter().enumerate().find_map(|(n, p)| {
        let (upper, lower) = sentence_starts(p);
        let how = match declared {
            Register::Standard if lower > 0 => "in lowercase",
            Register::Lowercase if upper > 0 => "with a capital",
            _ => return None,
        };
        Some(format!(
            "register mismatch: the receipt says {declared} but paragraph {} of the \
             draft under it starts a sentence {how} (#994) — ask for a recase",
            n + 1
        ))
    })
}

/// Notes for a reply or body: each `register:` receipt is checked against
/// every paragraph of the draft under it. A fenced block that is not under
/// a receipt is skipped whole, so a code example quoting the receipt format
/// is not read as one; text with no receipt produces no notes.
pub fn audit_register_receipts(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut notes = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(declared) = parse_receipt(lines[i]) {
            let (paragraphs, next) = draft_paragraphs(&lines, i);
            if let Some(declared) = declared {
                notes.extend(contradiction(declared, &paragraphs));
            }
            i = next;
        } else if is_fence(lines[i]) {
            i += 1;
            while i < lines.len() && !is_fence(lines[i]) {
                i += 1;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #994 verbatim: the recipient's thread reads "Hi, I wanted to check in
    /// on the proposal." (sentence case) and the draft came out all-lowercase
    /// — the exact draft the owner had to catch by eye. Flagged when the
    /// drafter classified correctly and still ignored itself, fenced or not.
    #[test]
    fn issue_994_lowercase_draft_for_a_capitalizing_recipient_is_flagged() {
        let draft = "hey casey, thanks for checking in. i'll have the proposal \
                     over tonight, let me know if thursday still works.";
        for reply in [
            format!("register: standard (she capitalizes), mirroring\n{draft}\n\nFiled the thread to people/casey.md."),
            format!("register: standard (she capitalizes), mirroring\n```\n{draft}\n```\nFiled the thread to people/casey.md."),
        ] {
            let notes = audit_register_receipts(&reply);
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains("receipt says standard"), "{}", notes[0]);
            assert!(notes[0].contains("paragraph 1 of the draft under it starts a sentence in lowercase"), "{}", notes[0]);
        }
    }

    /// Review finding: a paragraph that mixes registers (`hey Casey, ...
    /// I'll send it tonight.` under `standard`) is a partial violation the
    /// recipient still sees, not compliant "mixed evidence".
    #[test]
    fn a_single_contradicting_sentence_start_is_flagged() {
        let mixed = "register: standard (she capitalizes), mirroring\n```\n\
                     hey Casey, thanks for checking in. I'll send it tonight.\n```";
        let notes = audit_register_receipts(mixed);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("starts a sentence in lowercase"), "{}", notes[0]);
        let mixed = "register: lowercase (he types all-lowercase), mirroring\n```\n\
                     hey sam, running late. I'll be there by eight.\n```";
        let notes = audit_register_receipts(mixed);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("starts a sentence with a capital"), "{}", notes[0]);
    }

    /// Review finding: a wrong-register paragraph after a matching one must
    /// not hide behind the first paragraph — every paragraph is audited, and
    /// a fence bounds the draft so commentary after it is not.
    #[test]
    fn every_paragraph_of_the_draft_is_audited() {
        let unfenced = "register: lowercase (he types all-lowercase), mirroring\n\
                        hey Sam,\n\nThanks for checking in on the proposal.";
        let notes = audit_register_receipts(unfenced);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("paragraph 2 of the draft under it starts a sentence with a capital"), "{}", notes[0]);
        let fenced = "register: lowercase (he types all-lowercase), mirroring\n```\n\
                      hey sam, running late.\n\nThanks for checking in on the proposal.\n```\n\n\
                      Filed the thread to people/sam.md.";
        assert_eq!(audit_register_receipts(fenced).len(), 1);
        let fenced_ok = "register: lowercase (he types all-lowercase), mirroring\n```\n\
                         hey sam, running late.\n\nthanks for checking in on the proposal.\n```\n\n\
                         Filed the thread to people/sam.md.";
        assert!(audit_register_receipts(fenced_ok).is_empty());
    }

    #[test]
    fn matching_drafts_pass_silently() {
        let standard = "register: standard (she capitalizes), mirroring\n```\n\
                        Hey Casey, thanks for checking in. I'll have the proposal \
                        over tonight.\n\nTalk soon.\n```\nFiled to people/casey.md.";
        assert!(audit_register_receipts(standard).is_empty());
        let quoted = "register: lowercase (he types all-lowercase), mirroring\n\
                      > hey sam, running 10 late. order without me\n>\n> see you there in a bit\n\n\
                      Filed to people/sam.md.";
        assert!(audit_register_receipts(quoted).is_empty());
    }

    #[test]
    fn capitalized_draft_under_lowercase_or_defaulted_receipt_is_flagged() {
        let notes = audit_register_receipts("Register: lowercase (you asked)\n```\nHey Sam, running late. Order without me.\n```");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("receipt says lowercase") && notes[0].contains("with a capital"), "{}", notes[0]);
        let draft = "Hi Alex, quick question about the venue.";
        assert_eq!(audit_register_receipts(&format!("register: unknown, defaulting to lowercase (no samples on file)\n{draft}")).len(), 1);
        assert!(audit_register_receipts(&format!("register: unknown (no samples on file)\n{draft}")).is_empty());
    }

    /// Review finding: the receipt is the envelope. Ordinary answer traffic
    /// — a code example, a quoted inbound email, a loop status line, a fence
    /// that quotes the receipt format — carries no receipt and draws no note.
    #[test]
    fn text_without_a_receipt_is_not_audited() {
        for reply in [
            "run it like this:\n```\ncargo test -p augmentagent-cli\n```\nthen check the log.",
            "her last message:\n\nHi Alice,\n\nthanks for checking in on the proposal.\n\nhello again,\njo",
            "all quiet. no new threads since the last run.",
            "the receipt form is:\n```\nregister: standard (she capitalizes), mirroring\n```\nhey there, no thread on file for her.",
        ] {
            assert!(audit_register_receipts(reply).is_empty(), "{reply}");
        }
    }

    #[test]
    fn non_evidence_words_and_lines_are_left_alone() {
        let standard = "register: standard (he capitalizes), mirroring\n";
        // URL, handle or brand at a sentence start is not evidence; the sentence after it is.
        let url_first = format!("{standard}github.com/example/repo is failing on main. iPhone builds too. Can you take a look?");
        assert!(audit_register_receipts(&url_first).is_empty());
        // Greetings, sign-offs and list items are cased either way.
        let short_and_listed = format!("{standard}hi alice,\n\nHere is the plan for Thursday:\n- send the deck\n- book the room\n\nbest,\njo");
        assert!(audit_register_receipts(&short_and_listed).is_empty());
        // A code snippet inside an email body is content, not a sentence.
        let with_code = format!("{standard}Here is the failing call:\n```\ncargo test -p foo\n```\nThanks for taking a look.");
        assert!(audit_register_receipts(&with_code).is_empty());
        assert!(audit_register_receipts("register: standard\n\n").is_empty());
        // Prose that merely mentions the word is not a receipt.
        assert!(!is_register_receipt("the register: standard here"));
        assert!(!is_register_receipt("register: closed for the night"));
        assert!(is_register_receipt("  Register: Standard (she capitalizes)"));
    }
}
