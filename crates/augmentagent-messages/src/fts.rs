//! Full-text index over stored message text (#1100).
//!
//! `message_fts` is an FTS5 table (porter stemming, unicode folding, no
//! diacritics) whose rowid is the `message_index` rowid, maintained by the
//! same queue drain that maintains the index. It holds *prepared* text:
//! visible text of HTML mail, capped per message, attachment filenames only.
//! The input is the stored `emails` text, which was already redacted at the
//! persistence boundary; nothing else is read.
//!
//! Model- or user-supplied text never reaches `MATCH` directly:
//! [`match_expr`] quotes every token itself.

use augmentagent_store::rusqlite::{params, Connection};
use serde::Serialize;

/// Per-message cap on prepared body text.
pub const MAX_BODY_BYTES: usize = 32 * 1024;

/// Prepared columns for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsDoc {
    /// Conversation title + container (chat name, "#channel Server", note title).
    pub title: String,
    /// Email subject (email only).
    pub subject: String,
    pub body: String,
}

fn looks_like_html(s: &str) -> bool {
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

/// Visible text of an HTML document: tags removed, `<script>`/`<style>`/
/// `<head>` contents dropped, block tags as line breaks, common entities
/// decoded. Tag names and attribute values never reach the output.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let lower = html.to_ascii_lowercase();
    let mut i = 0usize;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(rel) = html[i..].find('>') else {
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
    let decoded = decode_entities(&out);
    let mut result = String::with_capacity(decoded.len());
    for line in decoded.lines() {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            result.push_str(&collapsed);
            result.push('\n');
        }
    }
    result
}

fn decode_entities(s: &str) -> String {
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

/// `[attachment: <type> <filename> <location>]` → `<filename>`.
fn attachment_filename(line: &str) -> Option<&str> {
    let inner = line
        .trim()
        .strip_prefix("[attachment:")?
        .strip_suffix(']')?;
    let mut parts = inner.split_whitespace();
    let first = parts.next()?;
    Some(if first.contains('/') {
        parts.next().unwrap_or(first)
    } else {
        first
    })
}

fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// Prepare the searchable text for one message.
pub fn prepare(
    platform: &str,
    conversation_title: Option<&str>,
    container: Option<&str>,
    subject: &str,
    body: &str,
) -> FtsDoc {
    let text = if looks_like_html(body) {
        html_to_text(body)
    } else {
        body.to_string()
    };
    let mut prepared = String::with_capacity(text.len().min(MAX_BODY_BYTES));
    for line in text.lines() {
        if prepared.len() >= MAX_BODY_BYTES {
            break;
        }
        match attachment_filename(line) {
            Some(name) => prepared.push_str(name),
            None => prepared.push_str(line),
        }
        prepared.push('\n');
    }
    let email = platform == "gmail";
    let title = if email {
        String::new()
    } else {
        [conversation_title.unwrap_or(""), container.unwrap_or("")]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    };
    FtsDoc {
        title,
        subject: if email {
            subject.to_string()
        } else {
            String::new()
        },
        body: cap(prepared, MAX_BODY_BYTES).trim_end().to_string(),
    }
}

pub(crate) fn upsert(
    c: &Connection,
    rowid: i64,
    doc: &FtsDoc,
) -> augmentagent_store::rusqlite::Result<()> {
    c.execute("DELETE FROM message_fts WHERE rowid = ?1", [rowid])?;
    c.execute(
        "INSERT INTO message_fts (rowid, title, subject, body) VALUES (?1, ?2, ?3, ?4)",
        params![rowid, doc.title, doc.subject, doc.body],
    )?;
    Ok(())
}

pub(crate) fn delete(c: &Connection, rowid: i64) -> augmentagent_store::rusqlite::Result<()> {
    c.execute("DELETE FROM message_fts WHERE rowid = ?1", [rowid])?;
    Ok(())
}

/// One full-text term from a parsed query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Term {
    Word(String),
    Phrase(String),
    Prefix(String),
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn render(t: &Term) -> Option<String> {
    match t {
        Term::Word(w) => (!w.trim().is_empty()).then(|| quote(w.trim())),
        Term::Phrase(p) => {
            let words: Vec<&str> = p.split_whitespace().collect();
            (!words.is_empty()).then(|| quote(&words.join(" ")))
        }
        Term::Prefix(p) => (!p.trim().is_empty()).then(|| format!("{}*", quote(p.trim()))),
    }
}

/// Build an FTS5 `MATCH` expression. Every token is a quoted string, so
/// FTS operators (`AND`, `OR`, `NOT`, `NEAR`, `*`, `^`, column filters) in
/// the input are literal text. `None` when there is no positive term (FTS5
/// cannot evaluate a pure negation).
pub fn match_expr(include: &[Term], exclude: &[Term]) -> Option<String> {
    let pos: Vec<String> = include.iter().filter_map(render).collect();
    if pos.is_empty() {
        return None;
    }
    let mut expr = pos.join(" AND ");
    for n in exclude.iter().filter_map(render) {
        expr = format!("({expr}) NOT {n}");
    }
    Some(expr)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextHit {
    pub message_id: String,
    pub rank: f64,
    pub snippet: String,
}

/// Column weights for bm25: title, subject, body.
pub const BM25: &str = "bm25(message_fts, 4.0, 2.0, 1.0)";

/// Ranked full-text search over the whole store (tests and diagnostics; the
/// query tool composes the same MATCH with structured filters).
pub fn search(
    c: &Connection,
    expr: &str,
    limit: usize,
) -> augmentagent_store::rusqlite::Result<Vec<TextHit>> {
    let sql = format!(
        "SELECT mi.message_id, {BM25} AS rank, \
                snippet(message_fts, 2, '[', ']', '…', 16) \
           FROM message_fts JOIN message_index mi ON mi.rowid = message_fts.rowid \
          WHERE message_fts MATCH ?1 \
          ORDER BY rank, mi.ts_ms DESC LIMIT ?2"
    );
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map(params![expr, limit as i64], |r| {
        Ok(TextHit {
            message_id: r.get(0)?,
            rank: r.get(1)?,
            snippet: r.get(2)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{check, drain};
    use augmentagent_store::{Email, Store};
    use std::time::Duration;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("t.db")).unwrap();
        (d, s)
    }

    fn put(s: &Store, id: &str, platform: &str, subject: &str, body: &str, date: &str) {
        s.upsert_email(&Email {
            message_id: id.into(),
            thread_id: Some(format!("t-{id}")),
            from: "Pat <pat@example.com>".into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: subject.into(),
            body: body.into(),
            date: date.into(),
            account_entity_id: None,
            platform: platform.into(),
            kind: "dm".into(),
        })
        .unwrap();
        drain(s, 100, Duration::ZERO).unwrap();
    }

    fn ids(s: &Store, include: &[Term], exclude: &[Term]) -> Vec<String> {
        let expr = match_expr(include, exclude).unwrap();
        s.with_conn(|c| search(c, &expr, 50))
            .unwrap()
            .into_iter()
            .map(|h| h.message_id)
            .collect()
    }

    fn w(s: &str) -> Term {
        Term::Word(s.into())
    }

    #[test]
    fn stemming_matches_inflections_and_diacritics_fold() {
        let (_d, s) = store();
        put(
            &s,
            "a",
            "gmail",
            "Video",
            "I was editing the launch video",
            "2026-01-01T00:00:00Z",
        );
        put(
            &s,
            "b",
            "gmail",
            "Cafe",
            "meet at the café tomorrow",
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(ids(&s, &[w("edit")], &[]), ["a"]);
        assert_eq!(ids(&s, &[w("edited")], &[]), ["a"]);
        assert_eq!(ids(&s, &[w("cafe")], &[]), ["b"]);
    }

    #[test]
    fn phrase_matches_only_adjacent_and_prefix_matches() {
        let (_d, s) = store();
        put(
            &s,
            "a",
            "gmail",
            "x",
            "the launch video is ready",
            "2026-01-01T00:00:00Z",
        );
        put(
            &s,
            "b",
            "gmail",
            "x",
            "the video for the launch",
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(ids(&s, &[Term::Phrase("launch video".into())], &[]), ["a"]);
        let mut both = ids(&s, &[Term::Prefix("laun".into())], &[]);
        both.sort();
        assert_eq!(both, ["a", "b"]);
        assert_eq!(ids(&s, &[w("video")], &[w("ready")]), ["b"]);
    }

    #[test]
    fn html_body_indexes_visible_text_only() {
        let (_d, s) = store();
        let html = r#"<html><head><style>.promo{color:red}</style><title>Hidden</title></head>
            <body><div class="promo" data-track="zebracode"><p>Your invoice &amp; receipt</p>
            <a href="https://example.com/unsubscribe">Manage preferences</a>
            <script>var trackingpixel = 1;</script></div></body></html>"#;
        put(&s, "a", "gmail", "Receipt", html, "2026-01-01T00:00:00Z");
        assert_eq!(ids(&s, &[w("invoice")], &[]), ["a"]);
        assert_eq!(
            ids(&s, &[w("preferences")], &[]),
            ["a"],
            "link text is searchable"
        );
        for hidden in [
            "promo",
            "zebracode",
            "href",
            "trackingpixel",
            "div",
            "hidden",
        ] {
            assert!(
                ids(&s, &[w(hidden)], &[]).is_empty(),
                "{hidden} must not be indexed"
            );
        }
    }

    #[test]
    fn oversize_body_is_capped_not_rejected() {
        let body = format!("start {} tailword", "filler ".repeat(20_000));
        let doc = prepare("gmail", None, None, "s", &body);
        assert!(doc.body.len() <= MAX_BODY_BYTES);
        assert!(doc.body.starts_with("start"));
        assert!(!doc.body.contains("tailword"));
        let multibyte = "é".repeat(MAX_BODY_BYTES);
        assert!(prepare("imessage", None, None, "", &multibyte).body.len() <= MAX_BODY_BYTES);
    }

    #[test]
    fn attachments_index_filename_only_and_titles_are_searchable() {
        let (_d, s) = store();
        s.upsert_email(&Email {
            message_id: "d".into(),
            thread_id: Some("40".into()),
            from: "Bob <discord:7>".into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: "Discord: Acme HQ #roadmap [Bob]".into(),
            body: "[attachment: image/png q3-plan.png https://cdn.example.com/secret-path/q3-plan.png]".into(),
            date: "2026-01-01T00:00:00Z".into(),
            account_entity_id: None,
            platform: "discord".into(),
            kind: "guild_channel".into(),
        })
        .unwrap();
        drain(&s, 10, Duration::ZERO).unwrap();
        assert_eq!(ids(&s, &[w("q3")], &[]), ["d"]);
        assert_eq!(ids(&s, &[w("roadmap")], &[]), ["d"], "channel title");
        assert_eq!(ids(&s, &[w("acme")], &[]), ["d"], "server name");
        assert!(ids(&s, &[w("cdn")], &[]).is_empty());
    }

    #[test]
    fn match_expr_quotes_every_token() {
        assert_eq!(
            match_expr(&[w("launch"), w("video")], &[]).as_deref(),
            Some("\"launch\" AND \"video\"")
        );
        assert_eq!(match_expr(&[w("a\"b")], &[]).as_deref(), Some("\"a\"\"b\""));
        assert_eq!(
            match_expr(&[Term::Prefix("lau".into())], &[]).as_deref(),
            Some("\"lau\"*")
        );
        assert_eq!(match_expr(&[], &[w("x")]), None);
        assert_eq!(match_expr(&[w("  ")], &[]), None);
    }

    #[test]
    fn fts_operator_words_are_literals() {
        let (_d, s) = store();
        put(
            &s,
            "a",
            "gmail",
            "x",
            "cats AND dogs",
            "2026-01-01T00:00:00Z",
        );
        put(&s, "b", "gmail", "x", "cats only", "2026-01-01T00:00:00Z");
        // Unquoted, `cats OR dogs` would match both; as literals it matches
        // documents containing all three words (OR is a stopword-free token).
        assert!(ids(&s, &[w("cats"), w("OR"), w("dogs")], &[]).is_empty());
        assert_eq!(ids(&s, &[w("NOT")], &[]), Vec::<String>::new());
        for evil in [
            "body:cats",
            "NEAR(cats dogs)",
            "cats*",
            "^cats",
            "\"",
            "(",
            ")",
            "{title body}: x",
            "-",
            ":",
        ] {
            let expr = match_expr(&[w(evil)], &[]).unwrap();
            s.with_conn(|c| search(c, &expr, 5))
                .unwrap_or_else(|e| panic!("{evil}: {e}"));
        }
    }

    #[test]
    fn arbitrary_input_never_makes_match_raise() {
        let (_d, s) = store();
        put(&s, "a", "gmail", "x", "hello world", "2026-01-01T00:00:00Z");
        let alphabet: Vec<char> = "ab \"'*^():-+{}[]NEARORAND\\\u{00e9}\u{4e2d}\t"
            .chars()
            .collect();
        let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..1500 {
            let len = next() % 12;
            let text: String = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let terms = match next() % 3 {
                0 => vec![Term::Word(text)],
                1 => vec![Term::Phrase(text)],
                _ => vec![Term::Prefix(text)],
            };
            if let Some(expr) = match_expr(&terms, &[Term::Word("zz\"".into())]) {
                s.with_conn(|c| search(c, &expr, 5))
                    .unwrap_or_else(|e| panic!("{expr}: {e}"));
            }
        }
    }

    #[test]
    fn update_replaces_and_delete_removes_index_entry() {
        let (_d, s) = store();
        put(
            &s,
            "a",
            "gmail",
            "x",
            "original words",
            "2026-01-01T00:00:00Z",
        );
        put(
            &s,
            "a",
            "gmail",
            "x",
            "replacement text",
            "2026-01-01T00:00:00Z",
        );
        assert!(ids(&s, &[w("original")], &[]).is_empty());
        assert_eq!(ids(&s, &[w("replacement")], &[]), ["a"]);
        s.with_conn(|c| c.execute("DELETE FROM emails WHERE messageId = 'a'", []))
            .unwrap();
        drain(&s, 10, Duration::ZERO).unwrap();
        assert!(ids(&s, &[w("replacement")], &[]).is_empty());
        let fts_rows: i64 = s
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM message_fts", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(fts_rows, 0);
    }

    #[test]
    fn ranking_prefers_title_hit_and_recency_breaks_ties() {
        let (_d, s) = store();
        put(
            &s,
            "body-old",
            "imessage",
            "iMessage: Friends",
            "the budget is ready",
            "2026-01-01T00:00:00Z",
        );
        put(
            &s,
            "body-new",
            "imessage",
            "iMessage: Friends",
            "the budget is ready",
            "2026-02-01T00:00:00Z",
        );
        put(
            &s,
            "title",
            "imessage",
            "iMessage: Budget crew",
            "see you there",
            "2025-01-01T00:00:00Z",
        );
        assert_eq!(
            ids(&s, &[w("budget")], &[]),
            ["title", "body-new", "body-old"]
        );
    }

    #[test]
    fn check_reports_rows_missing_from_fts() {
        let (_d, s) = store();
        put(&s, "a", "gmail", "x", "hello", "2026-01-01T00:00:00Z");
        assert!(check(&s).unwrap().is_complete());
        s.with_conn(|c| c.execute("DELETE FROM message_fts", []))
            .unwrap();
        let h = check(&s).unwrap();
        assert_eq!(h.fts_missing, 1);
        assert!(!h.is_complete());
    }

    #[test]
    fn query_plan_uses_the_fts_index_not_an_emails_scan() {
        let (_d, s) = store();
        put(&s, "a", "gmail", "x", "hello", "2026-01-01T00:00:00Z");
        let plan: Vec<String> = s
            .with_conn(|c| {
                let sql = format!(
                    "EXPLAIN QUERY PLAN SELECT mi.message_id, {BM25} AS rank FROM message_fts \
                     JOIN message_index mi ON mi.rowid = message_fts.rowid \
                     WHERE message_fts MATCH '\"hello\"' ORDER BY rank LIMIT 20"
                );
                let mut stmt = c.prepare(&sql)?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(3))?;
                rows.collect()
            })
            .unwrap();
        let joined = plan.join(" | ");
        assert!(joined.contains("VIRTUAL TABLE INDEX"), "{joined}");
        assert!(
            !joined.contains("SCAN emails") && !joined.contains("SCAN mi"),
            "{joined}"
        );
    }
}
