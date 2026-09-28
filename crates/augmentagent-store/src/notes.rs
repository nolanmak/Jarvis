//! Subject encoding for Apple Notes rows (#1060).
//!
//! The notes channel stores one `emails` row per note with the title and
//! folder packed into `subject`. Retrieval has to take them back out, so
//! writer and readers share these helpers rather than each re-deriving it.

const PREFIX: &str = "Apple Note: ";

/// `subject` for a note row: `Apple Note: <title> [<folder>]`.
pub fn note_subject(title: &str, folder: &str) -> String {
    format!("{PREFIX}{title} [{folder}]")
}

/// Inverse of [`note_subject`]. `None` when `subject` isn't a note subject;
/// the folder is empty when the suffix is missing (older rows).
pub fn note_title_folder(subject: &str) -> Option<(&str, &str)> {
    let rest = subject.strip_prefix(PREFIX)?.trim_end();
    match rest.strip_suffix(']').and_then(|r| r.rsplit_once(" [")) {
        Some((title, folder)) => Some((title.trim(), folder)),
        None => Some((rest, "")),
    }
}

/// Words from an inbound subject or sender name worth matching against
/// notes. Common words are dropped, but short ones survive: "Amy" and "Q3"
/// are exactly what a note gets titled after. [`token_matches`] is what
/// keeps them from over-matching.
pub fn title_match_tokens(subject: &str) -> Vec<String> {
    const SKIP: &str = " about all am an and any are as at be but by can did do for from fw fwd \
        get had has have he her hi him his how if in into is it its just let me more my need new \
        no not of ok on one or our out over please re see she so some thanks that the them then \
        they this to up us was we were what when who why will with you your ";
    let mut tokens: Vec<String> = Vec::new();
    for word in subject.split(|c: char| !c.is_alphanumeric()) {
        let w = word.to_lowercase();
        if w.chars().count() >= 2 && !SKIP.split(' ').any(|s| s == w) && !tokens.contains(&w) {
            tokens.push(w);
        }
    }
    tokens
}

/// Whether `needle` occurs in the already-lowercased `haystack`. Four
/// characters or more match as substrings, so "cabin" finds "cabins" and an
/// address finds itself mid-sentence. Shorter needles must be whole words,
/// or "q3" would hit "q30" and "amy" anything spelling it in passing.
pub fn token_matches(needle: &str, haystack: &str) -> bool {
    if needle.chars().count() >= 4 {
        return haystack.contains(needle);
    }
    haystack.split(|c: char| !c.is_alphanumeric()).any(|w| w == needle)
}

/// Epoch ms of a note row's `receivedAt`, which carries the note's own
/// `modified` timestamp and — unlike `firstSeenAt` — is rewritten on every
/// edit. `None` when it doesn't parse, so callers can fall back rather than
/// invent a date.
pub fn note_modified_ms(received_at: &str) -> Option<i64> {
    time::OffsetDateTime::parse(
        received_at.trim(),
        &time::format_description::well_known::Rfc3339,
    )
    .ok()
    .map(|t| t.unix_timestamp_nanos() as i64 / 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_ms_reads_the_offset() {
        let utc = note_modified_ms("2026-09-02T14:00:00Z").unwrap();
        assert_eq!(note_modified_ms("2026-09-02T10:00:00-04:00"), Some(utc));
        assert_eq!(note_modified_ms("not a date"), None);
    }

    #[test]
    fn title_folder_splits_on_the_last_bracket_group() {
        let subject = note_subject("Cabin plan [draft]", "Notes");
        assert_eq!(note_title_folder(&subject), Some(("Cabin plan [draft]", "Notes")));
        // Older rows predate the folder suffix; non-note subjects are skipped.
        assert_eq!(note_title_folder("Apple Note: Groceries"), Some(("Groceries", "")));
        assert_eq!(note_title_folder("Re: cabin plan"), None);
    }
}
