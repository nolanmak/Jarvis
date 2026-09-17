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
//! receipt is compared with the receipt, and a contradiction becomes a note
//! the caller posts as a visible `⚠️` line (the channel a refused `ATTACH:`
//! uses). The receipt line is the whole envelope: text with no receipt is
//! not audited, so a code example, a quoted inbound email or a loop's status
//! report never draws a note (guessing drafts from their shape was tried and
//! flagged ordinary answers). Where the recipient's own message is in hand —
//! an email reply's `--reply-to-body*` — the draft is also held to *their*
//! casing ([`audit_draft_against_sample`]), so a receipt that misclassifies
//! them (the #994 failure) vouches for nothing.
//!
//! Call sites: Discord replies via `attachments::prepare_answer_delivery`
//! and `/loop` results in `loops.rs`; email bodies via `gmail compose|
//! update-draft|send-now` in `main.rs`, refused before any Gmail write.

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

/// Does the text after `register:` look like the protocol's own receipt rather
/// than the start of a sentence?
///
/// Accepts the keyword alone, or the keyword followed only by a parenthetical
/// reason and short bookkeeping (`, mirroring`, `, defaulting to lowercase`).
/// Rejects the keyword followed by more prose, which is what an ordinary
/// sentence looks like.
fn has_receipt_shape(rest: &str) -> bool {
    let word_len = rest.chars().take_while(char::is_ascii_alphabetic).count();
    let mut tail = rest[word_len..].trim();
    // An optional parenthetical reason: "(she capitalizes)".
    if let Some(open) = tail.strip_prefix('(') {
        match open.split_once(')') {
            Some((_, after)) => tail = after.trim(),
            // An unclosed parenthesis is prose, not a receipt.
            None => return false,
        }
    }
    let tail = tail.trim_start_matches(',').trim();
    if tail.is_empty() {
        return true;
    }
    // Only the protocol's own short notes may follow.
    let t = tail.to_ascii_lowercase();
    t.starts_with("defaulting to") || t == "mirroring"
}

/// Parse one line as a receipt. `unknown` resolves to the default it names
/// (`defaulting to lowercase`); an `unknown` naming none decides nothing and
/// callers on a send path must refuse it.
///
/// The line must have the PROTOCOL's shape, not merely start with the word.
/// Review of #994: the Gmail path drops a first line that parses as a receipt,
/// and there is no drafter flag at that boundary — so "register: standard
/// rates apply from April" in a hand-composed mail would be silently eaten.
/// The protocol emits the keyword, then optionally a parenthetical reason and
/// short trailing notes; a sentence continuing into ordinary prose is body
/// text and must survive untouched.
fn parse_receipt(line: &str) -> Option<Option<Register>> {
    let trimmed = line.trim();
    let head = trimmed.get(..REGISTER_RECEIPT_PREFIX.len())?;
    if !head.eq_ignore_ascii_case(REGISTER_RECEIPT_PREFIX) {
        return None;
    }
    let rest = trimmed[REGISTER_RECEIPT_PREFIX.len()..].trim_start();
    if !has_receipt_shape(rest) {
        return None;
    }
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

/// True when `line` is a receipt that records NO decision: `register: unknown`
/// with no `defaulting to …`.
///
/// Review of #994: such a receipt was accepted and stripped, so a drafted mail
/// could reach Gmail carrying a receipt that vouched for nothing — the casing
/// bug this issue is about, wearing a badge that says it was checked. A send
/// path must refuse it rather than drop it.
pub fn is_undecided_receipt(line: &str) -> bool {
    matches!(parse_receipt(line), Some(None))
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
/// protocol's form); a run of `>` blockquote lines is bounded the same way;
/// anything else reads through to the next receipt or the end, commentary
/// included — that is what the fence exists to separate. With no receipt
/// (an email body checked against its inbound) the whole text is the draft.
fn draft_paragraphs<'a>(lines: &[&'a str], receipt_idx: Option<usize>) -> (Vec<Vec<&'a str>>, usize) {
    let mut i = receipt_idx.map_or(0, |r| r + 1);
    while i < lines.len() && is_break(lines[i]) {
        i += 1;
    }
    let fenced = receipt_idx.is_some() && i < lines.len() && is_fence(lines[i]);
    let quoted = receipt_idx.is_some() && !fenced && i < lines.len() && lines[i].trim_start().starts_with('>');
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

/// Casing at each sentence start of the paragraph: its first word, and every
/// word after a `.`/`!`/`?`. A sentence runs ACROSS a soft wrap, so a line
/// break is not a boundary. Only sentence-shaped paragraphs (three
/// or more words) are evidence — a greeting or a sign-off is cased either
/// way — a list marker demotes the item's first word, and URL/handle/
/// abbreviation/brand-shaped words (`github.com/x`, `@sam`, `e.g.`,
/// `iPhone`) are skipped: their casing says nothing about register.
fn sentence_starts(paragraph: &[&str]) -> (usize, usize) {
    let (mut upper, mut lower) = (0, 0);
    // Evidence is judged per PARAGRAPH, not per line, and a sentence runs
    // across a soft wrap.
    //
    // Review of #994: `at_start` used to reset on every physical line, so an
    // ordinary wrapped draft read as beginning a new sentence at each wrap
    // point and a correctly-cased email was flagged. The same audit gates
    // Gmail writes, so that would have REFUSED real mail unattended.
    //
    // The three-word floor moves with it: it exists so a greeting or a
    // sign-off is not evidence, and those are their own paragraphs. Applying
    // it per line also dropped short continuation lines, taking their
    // sentence-ending punctuation with them.
    if paragraph
        .iter()
        .flat_map(|l| l.split_whitespace())
        .filter(|w| w.chars().any(char::is_alphabetic))
        .count()
        < 3
    {
        return (0, 0);
    }
    let mut at_start = true;
    for line in paragraph {
        let words: Vec<&str> = line.split_whitespace().collect();
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

/// The first paragraph with a sentence start that contradicts `declared`
/// (by `who`: the receipt, or the recipient's own message), as a note. One
/// is enough: a `standard` recipient reading a draft that opens `hey Casey,
/// thanks for checking in. I'll send it tonight.` still gets the lowercase
/// opener, so mixed evidence is a violation, not a wash.
fn contradiction(who: &str, declared: Register, paragraphs: &[Vec<&str>]) -> Option<String> {
    paragraphs.iter().enumerate().find_map(|(n, p)| {
        let (upper, lower) = sentence_starts(p);
        let how = match declared {
            Register::Standard if lower > 0 => "in lowercase",
            Register::Lowercase if upper > 0 => "with a capital",
            _ => return None,
        };
        Some(format!(
            "register mismatch: {who} {declared} but paragraph {} of the draft \
             starts a sentence {how} (#994) — ask for a recase",
            n + 1
        ))
    })
}

/// The register a person writes in, from one message of theirs: its sentence
/// starts, read up to the first quoted line so the owner's earlier turn under
/// `On … wrote:` is not taken for theirs. Mixed or no evidence is `None`.
fn register_of(sample: &str) -> Option<Register> {
    let own: Vec<&str> = sample
        .lines()
        .take_while(|l| !l.trim_start().starts_with('>') && !l.trim_end().ends_with("wrote:"))
        .collect();
    match sentence_starts(&own) {
        (0, 0) => None,
        (_, 0) => Some(Register::Standard),
        (0, _) => Some(Register::Lowercase),
        _ => None,
    }
}

/// Note when `draft` contradicts the register `sample` — the recipient's own
/// message — is written in: the receipt-free check for a reply whose inbound
/// is in the payload, where the classification is not the drafter's word.
pub fn audit_draft_against_sample(sample: &str, draft: &str) -> Option<String> {
    let declared = register_of(sample)?;
    let (paragraphs, _) = draft_paragraphs(&draft.lines().collect::<Vec<_>>(), None);
    contradiction("the recipient writes in", declared, &paragraphs)
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
            let (paragraphs, next) = draft_paragraphs(&lines, Some(i));
            if let Some(declared) = declared {
                notes.extend(contradiction("the receipt says", declared, &paragraphs));
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

    /// #994 review: `sentence_starts` treated the first word of EVERY physical
    /// line as a sentence start, so an ordinary soft-wrapped draft read as
    /// beginning a lowercase sentence at each wrap point.
    ///
    /// That is not a cosmetic false positive. The same audit gates Gmail
    /// writes, so a correctly-cased, normally-wrapped email would have been
    /// REFUSED — unattended, on live traffic.
    #[test]
    fn a_soft_wrapped_line_does_not_start_a_new_sentence() {
        let wrapped = "register: standard (she capitalizes), mirroring\n\
                       Hello Casey, I wanted to let you know\n\
                       that the proposal is ready. It went out\n\
                       this morning with the revised figures.\n";
        assert!(
            audit_register_receipts(wrapped).is_empty(),
            "a wrapped standard draft must not be flagged: {:?}",
            audit_register_receipts(wrapped)
        );

        // The same text unwrapped must agree — wrapping cannot change the
        // verdict, which is the whole bug.
        let flat = "register: standard (she capitalizes), mirroring\n\
                    Hello Casey, I wanted to let you know that the proposal is \
                    ready. It went out this morning with the revised figures.\n";
        assert!(audit_register_receipts(flat).is_empty());

        // And a genuine lowercase sentence after a full stop is still caught,
        // wrapped or not — the fix must not blind the audit.
        let real = "register: standard (she capitalizes), mirroring\n\
                    Hello Casey, the proposal is ready. it went out\n\
                    this morning with the revised figures.\n";
        assert!(
            !audit_register_receipts(real).is_empty(),
            "a lowercase sentence start after a period is still a mismatch"
        );
    }
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
            assert!(notes[0].contains("paragraph 1 of the draft starts a sentence in lowercase"), "{}", notes[0]);
        }
    }

    /// #994 verbatim, held to the recipient's own message instead of a
    /// receipt: her "Hi, I wanted to check in on the proposal." is standard
    /// (the owner's quoted lowercase turn under `wrote:` does not count), so
    /// the all-lowercase draft is a mismatch; a sample with no evidence holds nothing.
    #[test]
    fn issue_994_draft_is_held_to_the_recipients_own_casing() {
        let inbound = "Hi,\n\nI wanted to check in on the proposal. Does Thursday still work?\n\n\
                       Thanks,\nCasey\n\nOn Tue, Sep 8, 2026, Jo <jo@example.com> wrote:\n\
                       > hey casey, sending it over this week";
        let lower = "hey casey, thanks for checking in. i'll have the proposal over tonight, \
                     let me know if thursday still works.";
        let note = audit_draft_against_sample(inbound, lower).expect("mismatch");
        assert!(note.contains("the recipient writes in standard"), "{note}");
        assert!(note.contains("paragraph 1 of the draft starts a sentence in lowercase"), "{note}");
        let cased = "Hey Casey, thanks for checking in. I'll have the proposal over tonight.";
        assert!(audit_draft_against_sample(inbound, cased).is_none());
        assert!(audit_draft_against_sample("ok\n\n> Hi, I wanted to check in on the proposal.", cased).is_none());
        assert!(audit_draft_against_sample("hey jo, are we still on for thursday?", cased).is_some());
    }

    /// Review finding: a paragraph that mixes registers (`hey Casey, ...
    /// I'll send it tonight.` under `standard`) is a partial violation the
    /// recipient still sees, not compliant "mixed evidence".
    #[test]
    fn a_single_contradicting_sentence_start_is_flagged() {
        for (reply, how) in [
            ("register: standard (she capitalizes), mirroring\n```\nhey Casey, thanks for checking in. I'll send it tonight.\n```", "in lowercase"),
            ("register: lowercase (he types all-lowercase), mirroring\n```\nhey sam, running late. I'll be there by eight.\n```", "with a capital"),
            ("Register: lowercase (you asked)\n```\nHey Sam, running late. Order without me.\n```", "with a capital"),
        ] {
            let notes = audit_register_receipts(reply);
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains(&format!("starts a sentence {how}")), "{}", notes[0]);
        }
    }

    /// Review finding: a wrong-register paragraph after a matching one must
    /// not hide behind the first paragraph — every paragraph is audited, and
    /// a fence bounds the draft so commentary after it is not.
    #[test]
    fn every_paragraph_of_the_draft_is_audited() {
        let unfenced = "register: lowercase (he types all-lowercase), mirroring\nhey Sam,\n\nThanks for checking in on the proposal.";
        let notes = audit_register_receipts(unfenced);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("paragraph 2 of the draft starts a sentence with a capital"), "{}", notes[0]);
        let fenced = "register: lowercase (he types all-lowercase), mirroring\n```\nhey sam, running late.\n\n{}\n```\n\nFiled the thread to people/sam.md.";
        assert_eq!(audit_register_receipts(&fenced.replace("{}", "Thanks for checking in on the proposal.")).len(), 1);
        assert!(audit_register_receipts(&fenced.replace("{}", "thanks for checking in on the proposal.")).is_empty());
    }

    #[test]
    fn quoted_drafts_and_unknown_receipts() {
        let quoted = "register: lowercase (he types all-lowercase), mirroring\n\
                      > hey sam, running 10 late. order without me\n>\n> see you there in a bit\n\n\
                      Filed to people/sam.md.";
        assert!(audit_register_receipts(quoted).is_empty());
        let draft = "Hi Alex, quick question about the venue.";
        assert_eq!(audit_register_receipts(&format!("register: unknown, defaulting to lowercase (no samples on file)\n{draft}")).len(), 1);
        assert!(audit_register_receipts(&format!("register: unknown (no samples on file)\n{draft}")).is_empty());
    }

    /// Review finding: the receipt is the envelope — a code example, a quoted
    /// inbound, a loop status line, a fence quoting the receipt format draw no note.
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

    /// URL/handle/brand sentence starts, greetings, sign-offs, list items
    /// and code snippets inside an email body are not evidence; prose that
    /// merely mentions the word is not a receipt.
    #[test]
    fn non_evidence_words_and_lines_are_left_alone() {
        for draft in [
            "github.com/example/repo is failing on main. iPhone builds too. Can you take a look?",
            "hi alice,\n\nHere is the plan for Thursday:\n- send the deck\n- book the room\n\nbest,\njo",
            "Here is the failing call:\n```\ncargo test -p foo\n```\nThanks for taking a look.",
            "",
        ] {
            assert!(audit_register_receipts(&format!("register: standard (he capitalizes), mirroring\n{draft}")).is_empty(), "{draft}");
        }
        assert!(!is_register_receipt("the register: standard here"));
        assert!(!is_register_receipt("register: closed for the night"));
        assert!(is_register_receipt("  Register: Standard (she capitalizes)"));
    }
}
