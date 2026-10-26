//! Last-line guard against prompt context escaping into an outbound body
//! (#1579).
//!
//! The draft prompt carries internal context the drafter must read and never
//! repeat: the #1060 notes hint naming the owner's private note titles, the
//! wiki-hint preamble, and the boundary tags around thread history, tone and
//! resolved asks. A model echoed it and the reply shipped; a prompt can only
//! lower that rate, this makes it impossible.
//!
//! An echoed hint sentence costs its line — the rest of an otherwise good
//! reply still ships. An echoed boundary tag costs the whole block it opens:
//! the confidential material is the block's *contents*, so dropping the tag
//! line alone would publish exactly what it was wrapping. Markers are built
//! from the very constants the generators render, so the guard cannot drift
//! away from the text it guards against.

use augmentagent_wiki::reader::{DRAFT_HINT_PREAMBLE, DRAFT_HINT_PREFER_WIKI};

use crate::prompt::{NOTES_HINT_PREFIX, NOTES_HINT_SUFFIX};

/// `(name, text)` pairs whose echo costs the line carrying it. The name is
/// what we log — a marker's text can carry the owner's private note titles,
/// so it never reaches a log line, an `actions.errorMessage`, or a Discord
/// card.
const LINE_MARKERS: &[(&str, &str)] = &[
    ("notes_hint_prefix", NOTES_HINT_PREFIX),
    ("notes_hint_suffix", NOTES_HINT_SUFFIX),
    ("wiki_hint_preamble", DRAFT_HINT_PREAMBLE),
    ("wiki_hint_prefer_wiki", DRAFT_HINT_PREFER_WIKI),
];

/// `(name, tag)` pairs whose echo costs everything from the opening tag's
/// line through the line closing it (to the end of the body when nothing
/// closes it). Matched on the `<tag` prefix because the archetype block
/// renders an `id` attribute. The inbound `<email>` block is deliberately
/// absent: its contents are the recipient's own message, so quoting it back
/// discloses nothing.
const BLOCK_TAGS: &[(&str, &str)] = &[
    ("wiki_hint_block", "wiki_hint"),
    ("thread_history_block", "thread_history"),
    ("tone_profile_block", "tone_profile"),
    ("draft_archetype_block", "draft_archetype"),
    ("resolved_asks_block", "resolved_asks"),
];

/// Does `line` both open and close `tag`, i.e. is it a whole block on one
/// line?
fn closes_on_same_line(line: &str, tag: &str) -> bool {
    match line.find(&format!("<{tag}")) {
        Some(open) => line[open..].contains(&format!("</{tag}>")),
        None => false,
    }
}

/// Result of [`scrub_internal_context`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubOutcome {
    /// The body to send. Byte-identical to the input when nothing matched.
    pub body: String,
    /// Names of the markers that matched, in marker-table order, deduped.
    /// Safe to log — never the matched text.
    pub markers: Vec<&'static str>,
    /// How many lines were dropped.
    pub dropped_lines: usize,
}

impl ScrubOutcome {
    /// No internal context was found.
    pub fn is_clean(&self) -> bool {
        self.markers.is_empty()
    }

    /// Nothing sendable is left. True both when internal context was all the
    /// body held and when the generator produced only whitespace to begin
    /// with: either way the caller must refuse the write rather than create a
    /// blank message, so this deliberately does not consult `markers`.
    pub fn is_empty(&self) -> bool {
        self.body.trim().is_empty()
    }

    /// Comma-separated marker names for a log line.
    pub fn marker_summary(&self) -> String {
        self.markers.join(",")
    }
}

/// Drop every line of `body` that repeats internal prompt context, and every
/// line of a block an echoed boundary tag opens.
///
/// A marker-free body comes back byte-identical, trailing newline and line
/// endings included: the prompt cache and the byte-equality tests on the
/// other outbound scrubbers depend on an untouched passthrough.
pub fn scrub_internal_context(body: &str) -> ScrubOutcome {
    let mut markers: Vec<&'static str> = Vec::new();
    for (name, text) in LINE_MARKERS {
        if body.contains(text) {
            markers.push(name);
        }
    }
    for (name, tag) in BLOCK_TAGS {
        if body.contains(&format!("<{tag}")) || body.contains(&format!("</{tag}>")) {
            markers.push(name);
        }
    }
    if markers.is_empty() {
        return ScrubOutcome {
            body: body.to_string(),
            markers,
            dropped_lines: 0,
        };
    }

    let mut kept = String::with_capacity(body.len());
    let mut dropped_lines = 0;
    let mut open_block: Option<&str> = None;
    for line in body.split_inclusive('\n') {
        if let Some(tag) = open_block {
            dropped_lines += 1;
            if line.contains(&format!("</{tag}>")) {
                open_block = None;
            }
            continue;
        }
        if let Some((_, tag)) = BLOCK_TAGS
            .iter()
            .find(|(_, tag)| line.contains(&format!("<{tag}")))
        {
            dropped_lines += 1;
            if !closes_on_same_line(line, tag) {
                open_block = Some(tag);
            }
            continue;
        }
        let stray_close = BLOCK_TAGS
            .iter()
            .any(|(_, tag)| line.contains(&format!("</{tag}>")));
        if stray_close || LINE_MARKERS.iter().any(|(_, text)| line.contains(text)) {
            dropped_lines += 1;
            continue;
        }
        kept.push_str(line);
    }

    ScrubOutcome {
        body: kept,
        markers,
        dropped_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::notes_hint;

    #[test]
    fn the_notes_hint_sentence_never_survives_into_the_body() {
        let hint = notes_hint(&["Quarterly planning".into(), "Venue shortlist".into()]);
        let body = format!("Happy to chat Thursday.\n\nBest,\nNolan\n{hint}\n");
        let out = scrub_internal_context(&body);
        assert_eq!(out.body, "Happy to chat Thursday.\n\nBest,\nNolan\n");
        assert_eq!(out.dropped_lines, 1);
        assert!(out.markers.contains(&"notes_hint_prefix"));
        assert!(!out.body.contains("Quarterly planning"));

        let crlf = format!("Line one\r\n{hint}\r\nLine two\r\n");
        assert_eq!(scrub_internal_context(&crlf).body, "Line one\r\nLine two\r\n");
    }

    /// The #1579 leak as it was delivered: the reply's closing paragraph was
    /// the hint sentence, typed here verbatim rather than through
    /// [`notes_hint`] so a rewording of the generator cannot quietly retire
    /// the case the issue reported.
    #[test]
    fn the_leaked_line_from_the_issue_is_dropped_verbatim() {
        let body = "Thanks for the nudge — Thursday at 2 works on my end.\n\n\
                    - The owner keeps notes of their own on this topic: \"Venue shortlist\". \
                    Treat it as a topic they are actively tracking.\n";
        let out = scrub_internal_context(body);
        assert_eq!(out.body, "Thanks for the nudge — Thursday at 2 works on my end.\n\n");
        assert!(!out.is_empty());
    }

    #[test]
    fn a_marker_free_body_comes_back_byte_identical() {
        for body in [
            "Happy to chat Thursday.",
            "Hi there\r\n\r\nSounds good.\r\n",
            "",
            "trailing newline kept\n",
        ] {
            let out = scrub_internal_context(body);
            assert_eq!(out.body, body, "passthrough mutated: {body:?}");
            assert!(out.is_clean());
            assert_eq!(out.dropped_lines, 0);
        }
    }

    #[test]
    fn a_body_that_is_nothing_but_internal_context_reports_empty() {
        let body = format!(
            "{}\n{}\n",
            notes_hint(&["Venue shortlist".into()]),
            DRAFT_HINT_PREAMBLE
        );
        let out = scrub_internal_context(&body);
        assert!(out.is_empty(), "expected empty, got {:?}", out.body);
        assert_eq!(out.dropped_lines, 2);
    }

    /// An echoed boundary tag means the block's *contents* are what leaks, so
    /// the whole block goes — tag lines included.
    #[test]
    fn an_echoed_block_takes_its_contents_with_it() {
        let out = scrub_internal_context(
            "Hello.\n<thread_history>\nconfidential prior message\n</thread_history>\nBest,\n",
        );
        assert_eq!(out.body, "Hello.\nBest,\n");
        assert_eq!(out.dropped_lines, 3);
        assert_eq!(out.marker_summary(), "thread_history_block");
    }

    /// The block tags are named here rather than shared as constants: their
    /// renderers write them inline in format strings. Pin them against those
    /// sources so a renamed or retired block fails here instead of quietly
    /// leaving an echo of it unguarded.
    #[test]
    fn every_block_tag_is_one_a_prompt_still_renders() {
        let renderers = [include_str!("prompt.rs"), include_str!("archetype.rs"), include_str!("resolve.rs")];
        for (name, tag) in BLOCK_TAGS {
            let opened = format!("<{tag}");
            assert!(renderers.iter().any(|s| s.contains(&opened)), "{name} is no longer rendered");
        }
    }

    /// A body with no prose is clean — no marker matched — yet has nothing to
    /// send, so it still reports empty and callers still refuse the write.
    #[test]
    fn a_whitespace_only_body_is_clean_but_empty() {
        let out = scrub_internal_context(" \n");
        assert!(out.is_clean());
        assert!(out.is_empty());
        assert_eq!(out.body, " \n");
    }

    /// Block bounds: an unclosed block takes the rest of the body, a close tag
    /// whose opener never appeared still goes, the archetype block's `id`
    /// attribute is matched, and a one-line block does not swallow what
    /// follows it.
    #[test]
    fn block_bounds_hold_for_unbalanced_attributed_and_single_line_tags() {
        for (body, want) in [
            ("Hello.\n<tone_profile>\nhow the owner writes\n", "Hello.\n"),
            ("Hello.\n</thread_history>\n", "Hello.\n"),
            (
                "Hi.\n<draft_archetype id=\"decline\">\nintent\n</draft_archetype>\n\
                 <resolved_asks>secret</resolved_asks>\nSee you then.\n",
                "Hi.\nSee you then.\n",
            ),
        ] {
            assert_eq!(scrub_internal_context(body).body, want, "{body:?}");
        }
    }
}
