//! HTML → visible-text conversion shared by every consumer of stored mail.
//!
//! Lifted out of `augmentagent-messages::fts` by #1366 so the Discord renderer
//! strips markup with the converter the index already runs on real mail rather
//! than a third hand-rolled stripper. Only render boundaries convert; persisted
//! and indexed copies keep the original. [`html_to_text`] is byte-identical to
//! the pre-#1366 FTS output; [`html_to_text_for_display`] adds reader extras.

/// Cheap sniff for "this body is HTML, not prose". A fixed tag list, so the
/// index's verdict on a body does not move; render boundaries use
/// [`contains_markup`].
pub fn looks_like_html(s: &str) -> bool {
    let head: String = s
        .chars()
        .take(4000)
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "<html", "<body", "<div", "<table", "<p>", "<p ", "<br", "<span", "</a>",
    ]
    .iter()
    .any(|t| head.contains(t))
}

/// True if `s` holds anything tag-shaped — `<name…>` or `</name…>`, anywhere.
///
/// Render boundaries must not leak markup, so they cannot use a fixed tag list:
/// `<article>Update</article>` is ordinary HTML mail [`looks_like_html`] misses,
/// and a body the sniffer misses is posted verbatim. A `<` with no tag name
/// after it — `a < b` — is arithmetic in prose, so it does not count.
pub fn contains_markup(s: &str) -> bool {
    let bytes = s.as_bytes();
    s.bytes().enumerate().any(|(i, b)| {
        if b != b'<' {
            return false;
        }
        let name_at = if bytes.get(i + 1) == Some(&b'/') {
            i + 2
        } else {
            i + 1
        };
        bytes.get(name_at).is_some_and(u8::is_ascii_alphabetic) && s[name_at..].contains('>')
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Full-text index: the pre-#1366 `fts::html_to_text` output, exactly.
    Index,
    /// Human-readable surface: link targets kept, paragraph breaks kept.
    Display,
}

/// Visible text for the full-text index: tags removed, `<script>`/`<style>`/
/// `<head>` contents dropped, block tags as line breaks, entities decoded,
/// blank lines dropped. Tag names and attribute values never reach the output.
pub fn html_to_text(html: &str) -> String {
    convert(html, Mode::Index)
}

/// As [`html_to_text`], plus what a reader needs and an index does not:
/// `<a href>` targets in ` (url)` form, paragraph structure as at most one
/// blank line, and the tail of an unterminated tag kept as prose.
pub fn html_to_text_for_display(html: &str) -> String {
    convert(html, Mode::Display)
}

fn convert(html: &str, mode: Mode) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    let bytes = html.as_bytes();
    // One entry per open `<a>`: its target, plus the output offset its text
    // starts at (so an anchor whose text is already the URL isn't doubled).
    let mut anchors: Vec<(String, usize)> = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(rel) = html[i..].find('>') else {
                // A bare `<` with no `>` after it is text, not a tag. A reader
                // wants the remainder; the index dropped it pre-#1366 and keeps
                // doing so — a truncated tail changes which documents match.
                if mode == Mode::Display {
                    out.push_str(&html[i..]);
                }
                break;
            };
            let tag = &lower[i + 1..i + rel];
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let closing = tag.starts_with('/');
            i += rel + 1;
            if !closing && matches!(name.as_str(), "script" | "style" | "head" | "title") {
                let end_tag = format!("</{name}");
                match lower[i..].find(&end_tag) {
                    Some(end) => {
                        i += end;
                        if let Some(gt) = html[i..].find('>') {
                            i += gt + 1;
                        }
                    }
                    None => break,
                }
                continue;
            }
            if name == "a" && mode == Mode::Display {
                if closing {
                    if let Some((href, start)) = anchors.pop() {
                        if out[start..].trim() != href {
                            out.push_str(&format!(" ({href})"));
                        }
                    }
                } else if let Some(href) = href_of(tag, &html[i - rel..i - 1]) {
                    anchors.push((href, out.len()));
                }
            }
            if matches!(
                name.as_str(),
                "br" | "p"
                    | "div"
                    | "tr"
                    | "li"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "table"
                    | "blockquote"
            ) {
                out.push('\n');
            } else {
                out.push(' ');
            }
        } else {
            let next = html[i..].find('<').map_or(bytes.len(), |n| i + n);
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    let decoded = decode_entities(&out, mode);
    let mut result = String::with_capacity(decoded.len());
    let mut pending_blank = false;
    for line in decoded.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            // Runs of empty lines — one per nested `<div>`/`<blockquote>` —
            // collapse to a single paragraph break for a reader. The index
            // drops them all, as it did pre-#1366.
            pending_blank = mode == Mode::Display && !result.is_empty();
            continue;
        }
        if pending_blank {
            result.push('\n');
            pending_blank = false;
        }
        result.push_str(&collapsed);
        result.push('\n');
    }
    result
}

/// `href` target of an `<a>`, matched on `lower` but read out of `raw` so the
/// URL keeps its case — same tag, and `to_ascii_lowercase` preserves byte
/// length, so the offsets agree. Empty, `#`-only and `mailto:` targets carry
/// nothing a reader needs.
fn href_of(lower: &str, raw: &str) -> Option<String> {
    let at = lower.find("href")?;
    let rest = raw[at + 4..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let value = match rest.chars().next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next()?,
        _ => rest.split_whitespace().next()?,
    }
    .trim();
    let keep = !value.is_empty() && value != "#" && !value.starts_with("mailto:");
    keep.then(|| value.to_string())
}

fn decode_entities(s: &str, mode: Mode) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let semi = rest.as_bytes().iter().take(12).position(|b| *b == b';');
        let Some(semi) = semi else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let rep = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" | "#160" => Some(' '),
            // #1366 — punctuation a mail composer emits by default; left raw it
            // reads as markup on the card. The index leaves these alone, as it
            // did pre-#1366, so its tokens do not move.
            "mdash" if mode == Mode::Display => Some('\u{2014}'),
            "ndash" if mode == Mode::Display => Some('\u{2013}'),
            "hellip" if mode == Mode::Display => Some('\u{2026}'),
            "lsquo" if mode == Mode::Display => Some('\u{2018}'),
            "rsquo" if mode == Mode::Display => Some('\u{2019}'),
            "ldquo" if mode == Mode::Display => Some('\u{201c}'),
            "rdquo" if mode == Mode::Display => Some('\u{201d}'),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match rep {
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
    fn anchor_keeps_its_target() {
        let linked = r#"<p><a href="https://example.com/x">click</a></p>"#;
        assert_eq!(
            html_to_text_for_display(linked).trim(),
            "click (https://example.com/x)"
        );
        // Self-describing anchors and targets a reader can't use stay bare.
        let same = r#"<p><a href="https://example.com">https://example.com</a></p>"#;
        assert_eq!(html_to_text_for_display(same).trim(), "https://example.com");
        assert_eq!(
            html_to_text_for_display(r#"<p><a href="mailto:peer@example.com">peer</a></p>"#).trim(),
            "peer"
        );
    }

    #[test]
    fn nested_blockquotes_collapse_to_one_paragraph_break() {
        let html = "<blockquote><div><blockquote><div><div>quoted</div></div>\
                    </blockquote></div></blockquote><p>reply</p>";
        assert_eq!(html_to_text_for_display(html), "quoted\n\nreply\n");
    }

    /// #1366 split one converter into two modes. Index mode must still produce
    /// exactly what `fts::html_to_text` did before the split, or already-indexed
    /// bodies disagree with newly indexed ones.
    #[test]
    fn index_mode_output_is_unchanged_by_the_display_additions() {
        assert_eq!(
            html_to_text(r#"<p><a href="https://example.com/x">click</a></p>"#),
            "click\n"
        );
        assert_eq!(html_to_text("<p>first</p><p>second</p>"), "first\nsecond\n");
        let raw = "<p>a&mdash;b &hellip; &rsquo;</p>";
        assert_eq!(html_to_text(raw), "a&mdash;b &hellip; &rsquo;\n");
        // Prose up to a bare `<` is kept for a reader; the index drops the tail,
        // since a truncated one changes which documents match.
        assert_eq!(html_to_text("<p>x</p>y < z and more"), "x\ny\n");
        let disp = html_to_text_for_display("<p>x</p>y < z and more");
        assert_eq!(disp, "x\ny < z and more\n");
        // Entities the pre-split converter did decode still decode.
        assert_eq!(html_to_text("<p>a&nbsp;&amp;b</p>"), "a &b\n");
    }

    #[test]
    fn markup_sniff_catches_tags_the_fixed_list_misses() {
        // The reported leak: HTML mail built from tags `looks_like_html` omits.
        assert!(contains_markup("<article>Update</article>"));
        assert!(!looks_like_html("<article>Update</article>"));
        // Prose with arithmetic is not markup.
        assert!(!contains_markup("a < b and 5 < 6, right?"));
        assert!(!contains_markup("plain text"));
        // Tag-shaped but never terminated: nothing to strip.
        assert!(!contains_markup("<article"));
    }
}
