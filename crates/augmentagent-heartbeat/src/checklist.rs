//! The operator's heartbeat checklist, `<wiki>/HEARTBEAT.md`.
//!
//! A missing or effectively-empty checklist skips the run before any model
//! call. OpenClaw only skipped the empty case and still paid for a run when
//! the file was missing (their #83143); here both are free.

use std::path::Path;

/// File name under the wiki root.
pub const FILE_NAME: &str = "HEARTBEAT.md";

/// The checklist text, or `None` when the file is missing, unreadable or
/// [effectively empty](is_effectively_empty).
pub fn load(wiki_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(wiki_root.join(FILE_NAME)).ok()?;
    (!is_effectively_empty(&text)).then_some(text)
}

/// True when every line is blank, an ATX heading, part of an HTML comment,
/// an empty list stub (`-`, `* [ ]`) or a code-fence marker, i.e. the file
/// carries no instruction worth a model call.
pub fn is_effectively_empty(text: &str) -> bool {
    strip_comments(text).lines().all(|line| {
        let line = line.trim();
        line.is_empty() || is_heading(line) || is_list_stub(line) || is_fence(line)
    })
}

/// Drop `<!-- ... -->` spans, including multi-line and unterminated ones.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// ATX heading: one or more `#` then whitespace or end of line.
fn is_heading(line: &str) -> bool {
    let after = line.trim_start_matches('#');
    after.len() < line.len() && (after.is_empty() || after.starts_with(char::is_whitespace))
}

/// `-`, `* [ ]`, `+ [x]` with nothing after.
fn is_list_stub(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(['-', '*', '+']) else {
        return false;
    };
    let rest = rest.trim();
    rest.is_empty() || matches!(rest, "[]" | "[ ]" | "[x]" | "[X]")
}

fn is_fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffolding_only_is_effectively_empty() {
        let text =
            "# Heartbeat\n\n<!-- add checks below -->\n## Inbox\n- \n- [ ]\n* [x]\n```\n```\n\n";
        assert!(is_effectively_empty(text));
        assert!(is_effectively_empty(""));
        assert!(is_effectively_empty("<!--\nmulti-line\ncomment\n-->\n#\n"));
    }

    #[test]
    fn any_real_line_is_not_empty() {
        assert!(!is_effectively_empty(
            "# Heartbeat\n- check calendar for conflicts\n"
        ));
        assert!(!is_effectively_empty(
            "Ping me if an investor email waits over a day"
        ));
        assert!(!is_effectively_empty("```\nstill an instruction\n```"));
        assert!(!is_effectively_empty("#hashtag is not a heading"));
    }

    #[test]
    fn load_returns_none_for_missing_or_empty_and_text_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);

        std::fs::write(dir.path().join(FILE_NAME), "# Heartbeat\n- [ ]\n").unwrap();
        assert_eq!(load(dir.path()), None);

        std::fs::write(
            dir.path().join(FILE_NAME),
            "# Heartbeat\n- watch for flight changes\n",
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).as_deref(),
            Some("# Heartbeat\n- watch for flight changes\n")
        );
    }
}
