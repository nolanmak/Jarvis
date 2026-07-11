//! Minimal TinyMCE-HTML → plain-text conversion for wiki ingest.
//!
//! Journal bodies are TinyMCE output: `<p>`, `<br>`, `<strong>/<em>`,
//! lists, and the occasional `<img>` (S3 URL). The wiki ingest call wants
//! readable text, not markup fidelity, so a small state machine beats a
//! full HTML parser dependency: strip tags, keep block boundaries as
//! newlines, mark images, decode the entities TinyMCE actually emits.
//! Journal text is sensitive — nothing in here may log its input.

/// Tags whose end (or self-closing occurrence) implies a line break.
/// `li` is handled separately: its *opening* emits the bullet+break.
const BLOCK_TAGS: &[&str] = &[
    "p", "div", "br", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6", "tr", "blockquote",
];

pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    // '<' and '>' are ASCII, so byte-offset slicing stays on char
    // boundaries no matter what multibyte text sits between them.
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            // Unterminated tag — emit the raw remainder verbatim.
            out.push_str(&rest[lt..]);
            rest = "";
            break;
        };
        let tag_body = &after[..gt];
        rest = &after[gt + 1..];

        let name = tag_body
            .trim_start_matches('/')
            .split([' ', '\t', '\n', '/'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_closing = tag_body.starts_with('/');

        if name == "img" {
            out.push_str("[image]");
        } else if name == "li" && !is_closing {
            out.push_str("\n- ");
        } else if BLOCK_TAGS.contains(&name.as_str()) && (is_closing || name == "br") {
            out.push('\n');
        }
    }
    out.push_str(rest);

    let decoded = decode_entities(&out);
    // Collapse 3+ consecutive newlines and trim.
    let mut result = String::with_capacity(decoded.len());
    let mut newlines = 0usize;
    for c in decoded.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                result.push(c);
            }
        } else {
            newlines = 0;
            result.push(c);
        }
    }
    result.trim().to_string()
}

/// The entity set TinyMCE emits (plus numeric escapes).
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        rest = &rest[idx..];
        let Some(semi) = rest[..rest.len().min(10)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| n.parse::<u32>().ok())
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_become_lines() {
        assert_eq!(
            html_to_text("<p>first</p><p>second</p>"),
            "first\nsecond"
        );
    }

    #[test]
    fn lists_bullets_and_breaks() {
        assert_eq!(
            html_to_text("<p>today:</p><ul><li>one</li><li>two</li></ul>"),
            "today:\n\n- one\n- two"
        );
    }

    #[test]
    fn inline_markup_stripped_entities_decoded() {
        assert_eq!(
            html_to_text("<p><strong>bold</strong> &amp; <em>calm</em>&nbsp;&#128512;</p>"),
            "bold & calm 😀"
        );
    }

    #[test]
    fn images_marked_not_dropped() {
        assert_eq!(
            html_to_text(r#"<p>pic:</p><img src="https://s3/x.png" alt="">"#),
            "pic:\n[image]"
        );
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(html_to_text("no markup at all"), "no markup at all");
    }

    #[test]
    fn angle_bracket_without_close_survives() {
        assert_eq!(html_to_text("a < b"), "a < b");
    }
}
