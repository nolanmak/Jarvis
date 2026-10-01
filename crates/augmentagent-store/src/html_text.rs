//! HTML → visible-text conversion shared by every consumer of stored mail.
//!
//! Lifted out of `augmentagent-messages::fts` by #1366 so the Discord renderer strips
//! markup with the converter the index already runs on real mail, not a third
//! hand-rolled stripper. Only render boundaries convert; persisted and indexed copies
//! keep the original. [`html_to_text`] is byte-identical to the pre-#1366 FTS output;
//! [`html_to_text_for_display`] adds reader extras.

/// Cheap sniff for "this body is HTML, not prose". A fixed tag list, so the
/// index's verdict does not move; render boundaries use [`contains_markup`].
pub fn looks_like_html(s: &str) -> bool {
    let head = s.chars().take(4000).collect::<String>().to_ascii_lowercase();
    ["<html", "<body", "<div", "<table", "<p>", "<p ", "<br", "<span", "</a>"]
        .iter()
        .any(|t| head.contains(t))
}

/// True if `s` holds anything tag-shaped — `<name…>` or `</name…>`, anywhere. Render
/// boundaries must not leak markup, so they cannot use a fixed tag list:
/// `<article>Update</article>` is ordinary HTML mail [`looks_like_html`] misses, and a
/// missed body is posted verbatim. A `<` with no tag name after it — `a < b` — is
/// arithmetic in prose, so it does not count.
pub fn contains_markup(s: &str) -> bool {
    let bytes = s.as_bytes();
    s.bytes().enumerate().any(|(i, b)| {
        if b != b'<' {
            return false;
        }
        let name_at = if bytes.get(i + 1) == Some(&b'/') { i + 2 } else { i + 1 };
        bytes.get(name_at).is_some_and(u8::is_ascii_alphabetic)
            && tag_end(bytes, i, Mode::Display).is_some()
    })
}

/// `Index` is the pre-#1366 `fts::html_to_text` output, exactly; `Display` adds
/// link targets, named entities and paragraph breaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Index,
    Display,
}

/// Visible text for the full-text index: tags removed, `<script>`/`<style>`/
/// `<head>` contents dropped, block tags as line breaks, the core five entities
/// decoded, blank lines dropped. Tag names and attributes never reach the output.
pub fn html_to_text(html: &str) -> String {
    convert(html, Mode::Index)
}

/// As [`html_to_text`], plus what a reader needs and an index does not: `<a href>`
/// targets in ` (url)` form, the named entities in [`LATIN1`]/[`SYMBOLS`],
/// paragraphs as at most one blank line, an unterminated tag's tail kept as prose.
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
            let Some(rel) = tag_end(bytes, i, mode) else {
                // A bare `<` with no `>` after it is text, not a tag. A reader wants
                // the remainder; the index dropped it pre-#1366 and keeps doing so,
                // since a truncated tail changes which documents match.
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
                    if let Some((href, at)) = anchors.pop() {
                        if out[at..].trim() != href {
                            out.push_str(&format!(" ({href})"));
                        }
                    }
                } else if let Some(href) = href_of(tag, &html[i - rel..i - 1]) {
                    anchors.push((href, out.len()));
                }
            }
            #[rustfmt::skip]
            let block = matches!(name.as_str(), "br" | "p" | "div" | "tr" | "li" | "h1" | "h2"
                | "h3" | "h4" | "table" | "blockquote");
            out.push(if block { '\n' } else { ' ' });
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
            // Runs of empty lines — one per nested `<div>`/`<blockquote>` — become
            // one paragraph break for a reader; the index drops them all.
            pending_blank = mode == Mode::Display && !result.is_empty();
            continue;
        }
        if std::mem::take(&mut pending_blank) {
            result.push('\n');
        }
        result.push_str(&collapsed);
        result.push('\n');
    }
    result
}

/// Offset from the `<` at `open` to the `>` closing that tag, `None` if never
/// terminated. `Display` skips a `>` inside a quoted attribute value, so
/// `<div title="1 > 0">` is one tag and not a tag plus the literal text `0">`; a
/// quote only opens a value right after `=`, so a stray apostrophe in prose cannot
/// swallow the rest of the mail. `Index` stops at the first `>` regardless —
/// pre-#1366 `fts::html_to_text` did, and the FTS table holds documents tokenized
/// that way, so reading quotes here would stop a query matching them until a full
/// reindex. The leak is render-side only.
fn tag_end(bytes: &[u8], open: usize, mode: Mode) -> Option<usize> {
    if mode == Mode::Index {
        return bytes[open..].iter().position(|b| *b == b'>');
    }
    let mut j = open + 1;
    let mut after_eq = false;
    while j < bytes.len() {
        let b = bytes[j];
        if b == b'>' {
            return Some(j - open);
        }
        if after_eq && (b == b'"' || b == b'\'') {
            j += bytes[j + 1..].iter().position(|c| *c == b)? + 1;
            after_eq = false;
        } else if b == b'=' {
            after_eq = true;
        } else if !b.is_ascii_whitespace() {
            after_eq = false;
        }
        j += 1;
    }
    None
}

/// `href` target of an `<a>`, matched on `lower` but read out of `raw` so the URL
/// keeps its case — same tag, and `to_ascii_lowercase` preserves byte length, so
/// the offsets agree. Empty, `#`-only and `mailto:` targets carry nothing.
fn href_of(lower: &str, raw: &str) -> Option<String> {
    let at = lower.find("href")?;
    let rest = raw[at + 4..].trim_start().strip_prefix('=')?.trim_start();
    let value = match rest.chars().next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next()?,
        _ => rest.split_whitespace().next()?,
    }
    .trim();
    let keep = !value.is_empty() && value != "#" && !value.starts_with("mailto:");
    keep.then(|| value.to_string())
}

/// The HTML 4 Latin-1 names in codepoint order from `U+00A0`, so position *is* the
/// character and no second column can drift out of step with the first.
const LATIN1: &str = "nbsp iexcl cent pound curren yen brvbar sect uml copy ordf laquo not shy reg \
    macr deg plusmn sup2 sup3 acute micro para middot cedil sup1 ordm raquo frac14 frac12 frac34 \
    iquest Agrave Aacute Acirc Atilde Auml Aring AElig Ccedil Egrave Eacute Ecirc Euml Igrave \
    Iacute Icirc Iuml ETH Ntilde Ograve Oacute Ocirc Otilde Ouml times Oslash Ugrave Uacute Ucirc \
    Uuml Yacute THORN szlig agrave aacute acirc atilde auml aring aelig ccedil egrave eacute ecirc \
    euml igrave iacute icirc iuml eth ntilde ograve oacute ocirc otilde ouml divide oslash ugrave \
    uacute ucirc uuml yacute thorn yuml";

/// The rest of what a composer reaches for: smart punctuation, quotes, symbols.
const SYMBOLS: &str = "mdash=— ndash=– hellip=… lsquo=‘ rsquo=’ ldquo=“ rdquo=” sbquo=‚ bdquo=„ \
    bull=• dagger=† Dagger=‡ permil=‰ lsaquo=‹ rsaquo=› trade=™ euro=€ minus=−";

fn named(ent: &str) -> Option<char> {
    match LATIN1.split_whitespace().position(|n| n == ent) {
        Some(i) => char::from_u32(0xA0 + i as u32),
        None => SYMBOLS
            .split_whitespace()
            .find_map(|p| p.strip_prefix(ent)?.strip_prefix('=')?.chars().next()),
    }
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
            // #1366 — `named` adds the accent and symbol entities a composer emits by
            // default (`R&eacute;sum&eacute; &copy;`); raw they read as markup on the
            // card. The index leaves them alone, as pre-#1366.
            #[rustfmt::skip]
            _ => ent.strip_prefix("#x").and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32)
                .or_else(|| (mode == Mode::Display).then(|| named(ent)).flatten()),
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
    fn display_mode_keeps_link_targets_and_paragraph_structure() {
        let d = html_to_text_for_display;
        let linked = r#"<p><a href="https://example.com/x">click</a></p>"#;
        assert_eq!(d(linked).trim(), "click (https://example.com/x)");
        // Self-describing anchors and targets a reader can't use stay bare.
        let same = r#"<p><a href="https://example.com">https://example.com</a></p>"#;
        assert_eq!(d(same).trim(), "https://example.com");
        assert_eq!(d(r#"<a href="mailto:p@example.com">peer</a>"#).trim(), "peer");
        // Runs of empty lines — one per nested wrapper — become one break.
        let nested = "<blockquote><div><blockquote><div><div>quoted</div></div>\
                      </blockquote></div></blockquote><p>reply</p>";
        assert_eq!(d(nested), "quoted\n\nreply\n");
        // The tail of an unterminated tag is prose a reader wants kept.
        assert_eq!(d("<p>x</p>y < z and more"), "x\ny < z and more\n");
    }

    /// #1366 split one converter into two modes. Index mode must still produce exactly
    /// what `fts::html_to_text` did, or indexed bodies disagree with new ones.
    #[test]
    fn index_mode_output_is_unchanged_by_the_display_additions() {
        let x = html_to_text;
        assert_eq!(x(r#"<p><a href="https://example.com/x">c</a></p>"#), "c\n");
        assert_eq!(x("<p>first</p><p>second</p>"), "first\nsecond\n");
        // Entities past the core five stay literal; the core five still decode.
        assert_eq!(x("<p>a&mdash;b &hellip; &eacute; &copy;</p>"), "a&mdash;b &hellip; &eacute; &copy;\n");
        assert_eq!(x("<p>a&nbsp;&amp;b</p>"), "a &b\n");
        // A `>` in a quoted attribute still ends the tag, and prose after a bare `<`
        // is still dropped — both as pre-#1366, since the FTS table holds bodies
        // tokenized that way and a truncated tail changes which documents match.
        assert_eq!(x(r#"<div title="1 > 0">Update</div>"#), "0\">Update\n");
        assert_eq!(x("<p>x</p>y < z and more"), "x\ny\n");
    }

    #[test]
    fn markup_sniff_catches_tags_the_fixed_list_misses() {
        // The reported leak: HTML mail built from tags `looks_like_html` omits.
        assert!(contains_markup("<article>Update</article>"));
        assert!(!looks_like_html("<article>Update</article>"));
        // Prose with arithmetic is not markup: `<` must be followed by a name
        // *and* a terminator, so no comparison or unterminated fragment counts.
        for prose in ["a < b and 5 < 6, right?", "a < b > c", "plain text", "<article"] {
            assert!(!contains_markup(prose), "{prose}");
        }
    }

    /// Accent, symbol and punctuation entities must not reach the card as source.
    #[test]
    fn display_mode_decodes_ordinary_named_entities() {
        let d = |h| html_to_text_for_display(h).trim().to_string();
        assert_eq!(d("<div>R&eacute;sum&eacute; &copy; 2026</div>"), "Résumé © 2026");
        assert_eq!(
            d("<p>&Uuml;ber cr&egrave;me &mdash; &euro;12 &bull; 20&deg;C &frac12; &trade;</p>"),
            "Über crème — €12 • 20°C ½ ™"
        );
        // Unknown names stay literal rather than silently losing the text.
        assert_eq!(d("<p>&notareal; x</p>"), "&notareal; x");
    }

    /// A `>` inside a quoted attribute value does not end the tag — finding the end
    /// with the first `>` leaked `0">Update` onto the card. Index mode keeps the old
    /// first-`>` behaviour on purpose; see `tag_end`.
    #[test]
    fn a_quoted_angle_bracket_does_not_end_the_tag() {
        let d = |h| html_to_text_for_display(h).trim().to_string();
        for html in [
            r#"<div title="1 > 0">Update</div>"#,
            r#"<div title='1 > 0'>Update</div>"#,
            r#"<div data-x="a>b" title="c>d">Update</div>"#,
        ] {
            assert_eq!(d(html), "Update", "{html}");
        }
        // An anchor whose earlier attribute hides a `>` still yields its target.
        assert_eq!(
            d(r#"<a title="x > y" href="https://example.com/p">go</a>"#),
            "go (https://example.com/p)"
        );
    }
}
