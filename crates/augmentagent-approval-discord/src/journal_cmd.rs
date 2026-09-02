//! `!journal` command parsing (#428).
//!
//! Kept as a pure function so the grammar is unit-testable without a
//! serenity harness; the event handler owns dispatch and I/O.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalCmd {
    /// `!journal` / `!journal help` — print usage.
    Usage,
    /// `!journal done [title]` — compose an entry from the recent
    /// conversation and save it.
    Done { title: Option<String> },
    /// `!journal <text>` — save the text verbatim as an entry.
    Text(String),
}

pub const JOURNAL_USAGE: &str = "\
**!journal** — save a ShadowNote journal entry\n\
`!journal <text>` — save the text as today's entry\n\
`!journal done [title]` — compose an entry from our recent conversation and save it\n\
Entries are encrypted like the app's own and show up in ShadowNote + the wiki.";

pub const JOURNAL_NOT_CONFIGURED: &str = "\
Journal write-back isn't configured on this box (SHADOWNOTE_* keys missing — see epic #425). \
Nothing was saved. Until it's set up, just reply normally and the regular wiki ingest will \
capture what you write.";

/// `None` = not a `!journal` command (word-bounded: `!journals` doesn't match).
pub fn parse_journal_command(text: &str) -> Option<JournalCmd> {
    let rest = text.strip_prefix("!journal")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let arg = rest.trim();
    Some(if arg.is_empty() || arg.eq_ignore_ascii_case("help") {
        JournalCmd::Usage
    } else if arg.eq_ignore_ascii_case("done") {
        JournalCmd::Done { title: None }
    } else if let Some((_, title)) = arg
        .split_once(char::is_whitespace)
        .filter(|(word, _)| word.eq_ignore_ascii_case("done"))
    {
        // The keyword is case-insensitive in every form: `DONE Friday` is a
        // titled done, not an entry whose text starts with "DONE".
        JournalCmd::Done {
            title: Some(title.trim().to_string()).filter(|t| !t.is_empty()),
        }
    } else {
        JournalCmd::Text(arg.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_help_are_usage() {
        assert_eq!(parse_journal_command("!journal"), Some(JournalCmd::Usage));
        assert_eq!(
            parse_journal_command("!journal help"),
            Some(JournalCmd::Usage)
        );
    }

    #[test]
    fn done_with_and_without_title() {
        assert_eq!(
            parse_journal_command("!journal done"),
            Some(JournalCmd::Done { title: None })
        );
        assert_eq!(
            parse_journal_command("!journal done Friday review"),
            Some(JournalCmd::Done {
                title: Some("Friday review".into())
            })
        );
        // Keyword case never matters (#438 codex review).
        assert_eq!(
            parse_journal_command("!journal DONE Friday"),
            Some(JournalCmd::Done {
                title: Some("Friday".into())
            })
        );
        assert_eq!(
            parse_journal_command("!journal Done\tweekly"),
            Some(JournalCmd::Done {
                title: Some("weekly".into())
            })
        );
        assert_eq!(
            parse_journal_command("!journal DONE"),
            Some(JournalCmd::Done { title: None })
        );
    }

    #[test]
    fn free_text_is_saved_verbatim() {
        assert_eq!(
            parse_journal_command("!journal today was calm and productive"),
            Some(JournalCmd::Text("today was calm and productive".into()))
        );
    }

    #[test]
    fn word_boundary_and_non_commands() {
        assert_eq!(parse_journal_command("!journals list"), None);
        assert_eq!(parse_journal_command("journal me"), None);
        // "done" embedded in text is text, not the done command
        assert_eq!(
            parse_journal_command("!journal donezo day"),
            Some(JournalCmd::Text("donezo day".into()))
        );
    }
}
