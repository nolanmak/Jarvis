//! HTML → visible-text conversion shared by every consumer of stored mail. Lifted out
//! of `augmentagent-messages::fts` by #1366 so the Discord renderer strips markup with
//! the converter the index already runs on real mail, not a third hand-rolled stripper.
//! Only render boundaries convert; persisted and indexed copies keep the original.

/// Cheap sniff for "this body is HTML, not prose". A fixed tag list, so the index's
/// verdict does not move; render boundaries use [`contains_markup`] instead.
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

/// `Index` is the pre-#1366 `fts::html_to_text` output, exactly; `Display` adds link
/// targets, the full named-entity set and paragraph breaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Index,
    Display,
}

/// Visible text for the full-text index: tags removed, `<script>`/`<style>`/`<head>`
/// contents dropped, block tags as line breaks, the core five entities decoded, blank
/// lines dropped. Tag names and attributes never reach the output.
pub fn html_to_text(html: &str) -> String {
    convert(html, Mode::Index)
}

/// As [`html_to_text`], plus what a reader needs and an index does not: `<a href>`
/// targets in ` (url)` form, every HTML 4 named entity, paragraphs as at most one
/// blank line, an unterminated tag's tail kept as prose.
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
/// `<div title="1 > 0">` is one tag and not a tag plus the literal text `0">`; a quote
/// only opens a value right after `=`, so a stray apostrophe in prose cannot swallow
/// the rest of the mail. `Index` stops at the first `>` regardless, as pre-#1366: the
/// FTS table holds documents tokenized that way, and reading quotes here would stop a
/// query matching them until a full reindex. The leak is render-side only.
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

/// HTML 4's contiguous name blocks — Latin-1, then upper- and lower-case Greek — each
/// introduced by `@<hex>`, the codepoint of the name that follows it. Position within a
/// block *is* the character, so no second column can drift out of step with the first;
/// `.` fills the unassigned slot at `U+03A2`.
const RUNS: &str = "@A0 nbsp iexcl cent pound curren yen brvbar sect uml copy ordf laquo not shy \
    reg macr deg plusmn sup2 sup3 acute micro para middot cedil sup1 ordm raquo frac14 frac12 \
    frac34 iquest Agrave Aacute Acirc Atilde Auml Aring AElig Ccedil Egrave Eacute Ecirc Euml \
    Igrave Iacute Icirc Iuml ETH Ntilde Ograve Oacute Ocirc Otilde Ouml times Oslash Ugrave \
    Uacute Ucirc Uuml Yacute THORN szlig agrave aacute acirc atilde auml aring aelig ccedil \
    egrave eacute ecirc euml igrave iacute icirc iuml eth ntilde ograve oacute ocirc otilde ouml \
    divide oslash ugrave uacute ucirc uuml yacute thorn yuml \
    @391 Alpha Beta Gamma Delta Epsilon Zeta Eta Theta Iota Kappa Lambda Mu Nu Xi Omicron Pi Rho \
    . Sigma Tau Upsilon Phi Chi Psi Omega \
    @3B1 alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho \
    sigmaf sigma tau upsilon phi chi psi omega";

/// HTML 4's remaining named characters as `name=char`: smart punctuation, currency,
/// letterlike symbols, arrows, operators, card suits. Scattered across Unicode, so spelled
/// out rather than run-encoded. The whole standard set — not a hand-picked few — is the
/// point: any omission renders as `&name;` source on the card, the leak #1366 is about.
/// `ensp`/`emsp`/`thinsp` are absent — a space cannot be a value here — and map to a
/// plain one below, which `convert` collapses with its neighbours anyway.
const PAIRS: &str = "OElig=Œ oelig=œ Scaron=Š scaron=š Yuml=Ÿ fnof=ƒ circ=ˆ tilde=˜ thetasym=ϑ \
    upsih=ϒ piv=ϖ ndash=– mdash=— lsquo=‘ rsquo=’ sbquo=‚ ldquo=“ rdquo=” bdquo=„ dagger=† \
    Dagger=‡ bull=• hellip=… permil=‰ prime=′ Prime=″ lsaquo=‹ rsaquo=› oline=‾ frasl=⁄ euro=€ \
    image=ℑ weierp=℘ real=ℜ trade=™ alefsym=ℵ larr=← uarr=↑ rarr=→ darr=↓ harr=↔ crarr=↵ lArr=⇐ \
    uArr=⇑ rArr=⇒ dArr=⇓ hArr=⇔ forall=∀ part=∂ exist=∃ empty=∅ nabla=∇ isin=∈ notin=∉ ni=∋ \
    prod=∏ sum=∑ minus=− lowast=∗ radic=√ prop=∝ infin=∞ ang=∠ and=∧ or=∨ cap=∩ cup=∪ int=∫ \
    there4=∴ sim=∼ cong=≅ asymp=≈ ne=≠ equiv=≡ le=≤ ge=≥ sub=⊂ sup=⊃ nsub=⊄ sube=⊆ supe=⊇ \
    oplus=⊕ otimes=⊗ perp=⊥ sdot=⋅ lceil=⌈ rceil=⌉ lfloor=⌊ rfloor=⌋ lang=〈 rang=〉 loz=◊ \
    spades=♠ clubs=♣ hearts=♥ diams=♦ zwnj=\u{200c} zwj=\u{200d} lrm=\u{200e} rlm=\u{200f}";

fn named(ent: &str) -> Option<char> {
    if !ent.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    if matches!(ent, "ensp" | "emsp" | "thinsp") {
        return Some(' ');
    }
    let mut cp = 0u32;
    for tok in RUNS.split_whitespace() {
        match tok.strip_prefix('@') {
            Some(hex) => cp = u32::from_str_radix(hex, 16).ok()?,
            None if tok == ent => return char::from_u32(cp),
            None => cp += 1,
        }
    }
    PAIRS.split_whitespace().find_map(|p| p.strip_prefix(ent)?.strip_prefix('=')?.chars().next())
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
            #[rustfmt::skip]
            _ => ent.strip_prefix("#x").and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32).or_else(|| (mode == Mode::Display).then(|| named(ent)).flatten()),
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

    /// #1366 split one converter into two modes. Index mode must still produce exactly
    /// what `fts::html_to_text` did, or indexed bodies disagree with new ones: entities
    /// past the core five stay literal, a `>` in a quoted attribute still ends the tag,
    /// and prose after a bare `<` is still dropped.
    #[test]
    fn index_mode_output_is_unchanged_by_the_display_additions() {
        let x = html_to_text;
        assert_eq!(x(r#"<p><a href="https://example.com/x">c</a></p>"#), "c\n");
        assert_eq!(x("<p>first</p><p>second</p>"), "first\nsecond\n");
        assert_eq!(x("<p>a&mdash; &rarr; &eacute; &ensp;</p>"), "a&mdash; &rarr; &eacute; &ensp;\n");
        assert_eq!(x("<p>a&nbsp;&amp;b</p>"), "a &b\n");
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

    /// Everything display mode owes a reader and an index does not — link targets,
    /// paragraph breaks, an unterminated tag's tail, a `>` inside a quoted attribute value
    /// (the first-`>` scan leaked the literal `0">Update` onto the card), and every HTML 4
    /// named entity: a hand-picked subset is what made `<div>Next &rarr; review</div>`
    /// render its own markup, so each block of the standard set is spot-checked.
    #[test]
    fn display_mode_renders_what_a_reader_needs() {
        #[rustfmt::skip]
        let cases = [
            (r#"<p><a href="https://example.com/x">click</a></p>"#, "click (https://example.com/x)"),
            // Self-describing anchors and targets a reader can't use stay bare.
            (r#"<a href="https://example.com">https://example.com</a>"#, "https://example.com"),
            (r#"<a href="mailto:p@example.com">peer</a>"#, "peer"),
            (r#"<a title="x > y" href="https://example.com/p">go</a>"#, "go (https://example.com/p)"),
            (r#"<div title="1 > 0">Update</div>"#, "Update"),
            (r#"<div data-x='a>b' title="c>d">Update</div>"#, "Update"),
            // Nested wrappers collapse to one break; an unterminated tag's tail is prose.
            ("<blockquote><div><blockquote><div>q</div></blockquote></div></blockquote><p>r</p>", "q\n\nr"),
            ("<p>x</p>y < z and more", "x\ny < z and more"),
            ("<div>Next &rarr; review</div>", "Next → review"),
            ("<div>R&eacute;sum&eacute; &copy; 2026</div>", "Résumé © 2026"),
            ("<p>&Uuml;ber cr&egrave;me &mdash; &euro;12 &bull; 20&deg;C &frac12;</p>", "Über crème — €12 • 20°C ½"),
            ("<p>&larr;&uarr;&darr;&harr;&rArr;&crarr;&sum;&radic;&ne;&sdot;&lang;x&rang;</p>", "←↑↓↔⇒↵∑√≠⋅〈x〉"),
            ("<p>&Omega;&alpha;&pi;&sigmaf;&Delta;&spades;&loz;&OElig;&prime;&trade;</p>", "ΩαπςΔ♠◊Œ′™"),
            // A fixed-width space collapses; an unknown name stays literal rather than
            // silently losing the text around it.
            ("<p>a&ensp;&emsp;b</p>", "a b"),
            ("<p>&notareal; x</p>", "&notareal; x"),
        ];
        for (html, want) in cases {
            assert_eq!(html_to_text_for_display(html).trim(), want, "{html}");
        }
    }
}
