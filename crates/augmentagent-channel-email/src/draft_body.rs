//! The email draft contract is plaintext. Older compose callers used HTML
//! break markers for paragraphs; translate those markers before serializing
//! a plaintext provider body. Do not interpret other markup or entities.
use std::{borrow::Cow, sync::OnceLock};

pub(crate) fn plaintext(body: &str) -> Cow<'_, str> {
    // Recognize the compact legacy paragraph shape, not arbitrary prose
    // mentioning a literal HTML tag or plaintext with real paragraph breaks.
    if body.trim_end_matches(['\r', '\n']).contains(['\r', '\n']) {
        return Cow::Borrowed(body);
    }
    static PARAGRAPH: OnceLock<regex::Regex> = OnceLock::new();
    if !PARAGRAPH
        .get_or_init(|| {
            regex::Regex::new(r"(?i)<br[ \t]*/?[ \t]*>[ \t]*<br[ \t]*/?[ \t]*>").unwrap()
        })
        .is_match(body)
    {
        return Cow::Borrowed(body);
    }
    static BREAK: OnceLock<regex::Regex> = OnceLock::new();
    BREAK
        .get_or_init(|| regex::Regex::new(r"(?i)<br[ \t]*/?[ \t]*>").unwrap())
        .replace_all(body, "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draft_break_conversion_preserves_plaintext_and_unrelated_characters() {
        for body in [
            "First\n\nSecond",
            "1 < 2 & 3 > 2",
            "<pre>literal</pre> &lt;br/&gt;",
            "Unicode café",
            "A literal <br/> example",
            "Text < & literal <br/>\n\nSecond paragraph",
        ] {
            assert_eq!(plaintext(body), body);
        }
        assert_eq!(
            plaintext("First<br><br/>Second<BR />Third"),
            "First\n\nSecond\nThird"
        );
    }
}
