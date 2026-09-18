//! `search_messages`: Gmail-style operator grammar over the message index (#1099).
//!
//! `parse` is pure and total; `plan` turns a parsed query into one SQL
//! statement whose every value is a bound parameter, with full-text terms
//! routed through [`crate::fts::match_expr`] so nothing the model or the user
//! typed can change the shape of the SQL or of the FTS expression.
//!
//! Person references resolve through [`crate::people`]. An ambiguous
//! reference returns no rows plus the candidates: the tool never guesses.

use augmentagent_store::rusqlite::types::Value as SqlValue;
use augmentagent_store::rusqlite::{Connection, Row};
use serde::Serialize;

use crate::fts::{self, Term, BM25};
use crate::people::{self, MatchKind, PersonMatch};

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 50;
const SNIPPET_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Relevance,
    Newest,
    Oldest,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Query {
    pub with: Vec<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub platforms: Vec<String>,
    pub not_platforms: Vec<String>,
    pub kinds: Vec<String>,
    pub not_kinds: Vec<String>,
    pub server: Option<String>,
    pub channel: Option<String>,
    pub thread: Option<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub has_attachment: bool,
    pub latest: bool,
    pub sort: Option<Sort>,
    pub include: Vec<Term>,
    pub exclude: Vec<Term>,
}

impl Query {
    fn is_empty(&self) -> bool {
        *self == Query::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub token: String,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (in `{}`)", self.message, self.token)
    }
}
impl std::error::Error for ParseError {}

pub const OPERATORS: &[&str] = &[
    "with", "from", "to", "in", "is", "server", "channel", "thread", "after", "before", "on",
    "has", "sort",
];

/// Whitespace-separated tokens, honouring double quotes (`server:"Acme HQ"`,
/// `"exact phrase"`). An unbalanced quote runs to the end of the input.
fn tokenize(q: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in q.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(s: &str) -> (String, bool) {
    let trimmed = s.trim_matches('"');
    (trimmed.to_string(), s.starts_with('"'))
}

/// `YYYY-MM-DD`, RFC 3339, or a relative `7d` / `3w` / `6m` / `1y`.
/// Returns (start_ms, end_ms) for the value's day when it is a bare date.
fn parse_date(raw: &str, now_ms: i64) -> Option<(i64, i64)> {
    use chrono::{DateTime, Duration, NaiveDate, Utc};
    let v = raw.trim();
    if let Some(rest) = v.strip_suffix(['d', 'w', 'm', 'y']) {
        if let Ok(n) = rest.parse::<i64>() {
            let unit = v.chars().last()?;
            let days = match unit {
                'd' => n,
                'w' => n * 7,
                'm' => n * 30,
                _ => n * 365,
            };
            let start = DateTime::<Utc>::from_timestamp_millis(now_ms)? - Duration::days(days);
            return Some((start.timestamp_millis(), now_ms));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        let start = d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis();
        return Some((start, start + 86_400_000));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        let ms = t.timestamp_millis();
        return Some((ms, ms));
    }
    None
}

const KINDS: &[&str] = &[
    "dm", "group", "channel", "note", "email", "meeting", "other",
];

pub fn parse(input: &str) -> Result<Query, ParseError> {
    parse_at(input, chrono::Utc::now().timestamp_millis())
}

pub fn parse_at(input: &str, now_ms: i64) -> Result<Query, ParseError> {
    let mut q = Query::default();
    for raw in tokenize(input) {
        let err = |msg: &str| ParseError {
            token: raw.clone(),
            message: msg.to_string(),
        };
        let (negated, body) = match raw.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest.to_string()),
            _ => (false, raw.clone()),
        };
        // An operator is `name:value` where name is a known operator; a colon
        // inside a quoted string or an unknown prefix stays free text.
        let op_split = body.find(':').filter(|_| !body.starts_with('"')).map(|i| {
            let (a, b) = body.split_at(i);
            (a.to_ascii_lowercase(), b[1..].to_string())
        });
        match op_split {
            Some((name, value)) if OPERATORS.contains(&name.as_str()) => {
                let (value, _) = unquote(&value);
                if value.is_empty() {
                    return Err(err("operator needs a value"));
                }
                match name.as_str() {
                    "with" => q.with.push(value),
                    "from" => q.from.push(value),
                    "to" => q.to.push(value),
                    "in" => {
                        let list = value
                            .split(',')
                            .map(|s| s.trim().to_ascii_lowercase())
                            .filter(|s| !s.is_empty());
                        if negated {
                            q.not_platforms.extend(list);
                        } else {
                            q.platforms.extend(list);
                        }
                    }
                    "is" => {
                        let v = value.to_ascii_lowercase();
                        if v == "latest" {
                            q.latest = true;
                        } else if KINDS.contains(&v.as_str()) {
                            if negated {
                                q.not_kinds.push(v);
                            } else {
                                q.kinds.push(v);
                            }
                        } else {
                            return Err(err(&format!(
                                "unknown `is:` value; expected latest or one of {}",
                                KINDS.join(", ")
                            )));
                        }
                    }
                    "server" => q.server = Some(value),
                    "channel" => q.channel = Some(value),
                    "thread" => q.thread = Some(value),
                    "after" | "before" | "on" => {
                        let (start, end) = parse_date(&value, now_ms).ok_or_else(|| {
                            err("date must be YYYY-MM-DD, ISO-8601, or 7d/3w/6m/1y")
                        })?;
                        match name.as_str() {
                            "after" => q.after = Some(start),
                            "before" => q.before = Some(start),
                            _ => {
                                q.after = Some(start);
                                q.before = Some(end);
                            }
                        }
                    }
                    "has" => match value.to_ascii_lowercase().as_str() {
                        "attachment" => q.has_attachment = true,
                        "link" => q.include.push(Term::Prefix("http".into())),
                        _ => return Err(err("unknown `has:` value; expected attachment or link")),
                    },
                    "sort" => {
                        q.sort = Some(match value.to_ascii_lowercase().as_str() {
                            "relevance" => Sort::Relevance,
                            "newest" => Sort::Newest,
                            "oldest" => Sort::Oldest,
                            _ => return Err(err("sort must be relevance, newest or oldest")),
                        })
                    }
                    _ => unreachable!("operator list and match arms agree"),
                }
            }
            Some((name, _))
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic()) =>
            {
                return Err(err(&format!(
                    "unknown operator `{name}:`; known operators: {}",
                    OPERATORS.join(", ")
                )));
            }
            _ => {
                let (text, was_quoted) = unquote(&body);
                if text.is_empty() {
                    continue;
                }
                let term = if was_quoted {
                    Term::Phrase(text)
                } else if let Some(prefix) = text.strip_suffix('*') {
                    Term::Prefix(prefix.to_string())
                } else {
                    Term::Word(text)
                };
                if negated {
                    q.exclude.push(term);
                } else {
                    q.include.push(term);
                }
            }
        }
    }
    if q.is_empty() {
        return Err(ParseError {
            token: input.to_string(),
            message: "empty query: give at least one word or operator".into(),
        });
    }
    Ok(q)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ambiguity {
    /// The reference as typed.
    pub query: String,
    pub candidates: Vec<PersonMatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub message_id: String,
    pub thread_id: String,
    pub platform: String,
    pub conv_kind: String,
    pub conversation_title: Option<String>,
    pub container: Option<String>,
    pub sender_label: Option<String>,
    pub sender_handle: String,
    pub person: Option<String>,
    pub from_me: bool,
    pub timestamp: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    pub total_estimate: i64,
    pub next_offset: Option<usize>,
    /// How the query was understood, so a wrong reading is visible.
    pub interpreted_as: Query,
    /// Non-empty when a person reference matched several people: no rows are
    /// returned and the caller must disambiguate rather than pick.
    pub ambiguous: Vec<Ambiguity>,
    /// Person references that matched no page and were used as raw handles.
    pub unresolved: Vec<String>,
}

struct Sql {
    filters: Vec<String>,
    params: Vec<SqlValue>,
}

impl Sql {
    fn bind(&mut self, v: impl Into<SqlValue>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }

    fn in_list(&mut self, values: &[String]) -> String {
        let marks: Vec<String> = values.iter().map(|v| self.bind(v.clone())).collect();
        marks.join(", ")
    }
}

/// Neutralize LIKE wildcards in a user-supplied value: `%` and `_` typed in
/// a `server:`/`channel:` value are literal characters, not "match anything".
pub(crate) fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Handles for a person reference, and whether a person page claimed it.
type ResolvedRef = (Vec<String>, bool);
/// Everything `plan` produces: rows SQL, count SQL, bound params, ambiguous
/// references, references used as raw handles.
type PlannedQuery = (String, String, Vec<SqlValue>, Vec<Ambiguity>, Vec<String>);

/// Resolve one person reference to its handles. Returns `Err(candidates)`
/// when the reference is ambiguous.
fn person_handles(
    c: &Connection,
    reference: &str,
) -> augmentagent_store::rusqlite::Result<Result<ResolvedRef, Vec<PersonMatch>>> {
    let matches = people::resolve_person(c, reference)?;
    match matches.len() {
        0 => Ok(Ok((vec![crate::handles::canonical(reference)], false))),
        1 => {
            let m = &matches[0];
            let resolved = m.resolved;
            let handles = if m.kind == MatchKind::Handle && !resolved {
                vec![m.key.clone()]
            } else {
                people::handles_for(c, &m.key)?
            };
            Ok(Ok((handles, resolved)))
        }
        _ => Ok(Err(matches)),
    }
}

/// Build the SQL for a parsed query. Every value is bound.
pub fn plan(
    c: &Connection,
    q: &Query,
    limit: usize,
    offset: usize,
) -> augmentagent_store::rusqlite::Result<PlannedQuery> {
    let mut sql = Sql {
        filters: Vec::new(),
        params: Vec::new(),
    };
    let mut ambiguous = Vec::new();
    let mut unresolved = Vec::new();
    let match_expr = fts::match_expr(&q.include, &q.exclude);
    if let Some(expr) = &match_expr {
        // FTS5 wants the table name on the left of MATCH; an alias is not
        // accepted there, so the joins below use `message_fts` unaliased.
        let mark = sql.bind(expr.clone());
        sql.filters.push(format!("message_fts MATCH {mark}"));
    }

    let conversation_clause = |sql: &mut Sql, handles: &[String]| {
        let list = sql.in_list(handles);
        sql.filters.push(format!(
            "mi.conversation_id IN (SELECT conversation_id FROM message_index \
             WHERE sender_handle IN ({list}) OR counterpart_handle IN ({list}))"
        ));
    };

    for reference in q.with.iter().chain(q.to.iter()) {
        match person_handles(c, reference)? {
            Ok((handles, resolved)) => {
                if !resolved {
                    unresolved.push(reference.clone());
                }
                conversation_clause(&mut sql, &handles);
            }
            Err(candidates) => ambiguous.push(Ambiguity {
                query: reference.clone(),
                candidates,
            }),
        }
    }
    if !q.to.is_empty() {
        sql.filters.push("mi.from_me = 1".into());
    }
    for reference in &q.from {
        if reference.eq_ignore_ascii_case("me") {
            sql.filters.push("mi.from_me = 1".into());
            continue;
        }
        match person_handles(c, reference)? {
            Ok((handles, resolved)) => {
                if !resolved {
                    unresolved.push(reference.clone());
                }
                let list = sql.in_list(&handles);
                sql.filters.push(format!("mi.sender_handle IN ({list})"));
            }
            Err(candidates) => ambiguous.push(Ambiguity {
                query: reference.clone(),
                candidates,
            }),
        }
    }
    if !q.platforms.is_empty() {
        let list = sql.in_list(&q.platforms);
        sql.filters.push(format!("mi.platform IN ({list})"));
    }
    if !q.not_platforms.is_empty() {
        let list = sql.in_list(&q.not_platforms);
        sql.filters.push(format!("mi.platform NOT IN ({list})"));
    }
    if !q.kinds.is_empty() {
        let list = sql.in_list(&q.kinds);
        sql.filters.push(format!("mi.conv_kind IN ({list})"));
    }
    if !q.not_kinds.is_empty() {
        let list = sql.in_list(&q.not_kinds);
        sql.filters.push(format!("mi.conv_kind NOT IN ({list})"));
    }
    if let Some(server) = &q.server {
        let mark = sql.bind(format!("%{}%", like_escape(server)));
        sql.filters
            .push(format!("mi.container LIKE {mark} ESCAPE '\\'"));
    }
    if let Some(channel) = &q.channel {
        let mark = sql.bind(format!(
            "%{}%",
            like_escape(channel.trim_start_matches('#'))
        ));
        sql.filters
            .push(format!("mi.conversation_title LIKE {mark} ESCAPE '\\'"));
    }
    if let Some(thread) = &q.thread {
        let mark = sql.bind(thread.clone());
        sql.filters.push(format!("mi.conversation_id = {mark}"));
    }
    if let Some(after) = q.after {
        let mark = sql.bind(after);
        sql.filters.push(format!("mi.ts_ms >= {mark}"));
    }
    if let Some(before) = q.before {
        let mark = sql.bind(before);
        sql.filters.push(format!("mi.ts_ms < {mark}"));
    }
    if q.has_attachment {
        sql.filters.push("mi.has_attachment = 1".into());
    }

    let from = if match_expr.is_some() {
        "FROM message_fts JOIN message_index mi ON mi.rowid = message_fts.rowid"
    } else {
        "FROM message_index mi"
    };
    let where_clause = if sql.filters.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", sql.filters.join(" AND "))
    };
    let sort = q.sort.unwrap_or(if match_expr.is_some() && !q.latest {
        Sort::Relevance
    } else {
        Sort::Newest
    });
    let order = match sort {
        Sort::Relevance if match_expr.is_some() => format!("{BM25} ASC, mi.ts_ms DESC"),
        Sort::Oldest => "mi.ts_ms ASC".into(),
        _ => "mi.ts_ms DESC".into(),
    };
    let limit = if q.latest {
        1
    } else {
        limit.clamp(1, MAX_LIMIT)
    };
    let count_sql = format!("SELECT COUNT(*) {from}{where_clause}");
    let rows_sql = format!(
        "SELECT mi.rowid, mi.message_id, mi.conversation_id, mi.platform, mi.conv_kind, \
                mi.conversation_title, mi.container, mi.sender_label, mi.sender_handle, \
                mp.person_key, mi.from_me, mi.ts_ms \
         {from} LEFT JOIN message_people mp ON mp.handle = mi.sender_handle{where_clause} \
         ORDER BY {order} LIMIT {} OFFSET {}",
        limit,
        offset.min(10_000),
    );
    Ok((rows_sql, count_sql, sql.params, ambiguous, unresolved))
}

fn row_to_hit(row: &Row<'_>) -> augmentagent_store::rusqlite::Result<(i64, Hit)> {
    let ts: i64 = row.get(11)?;
    Ok((
        row.get(0)?,
        Hit {
            message_id: row.get(1)?,
            thread_id: row.get(2)?,
            platform: row.get(3)?,
            conv_kind: row.get(4)?,
            conversation_title: row.get(5)?,
            container: row.get(6)?,
            sender_label: row.get(7)?,
            sender_handle: row.get(8)?,
            person: row.get(9)?,
            from_me: row.get::<_, i64>(10)? != 0,
            timestamp: chrono::DateTime::from_timestamp_millis(ts)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            snippet: String::new(),
        },
    ))
}

pub fn search(
    c: &Connection,
    query: &str,
    limit: Option<usize>,
    offset: usize,
) -> anyhow::Result<SearchResponse> {
    let q = parse(query)?;
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let (rows_sql, count_sql, params, ambiguous, unresolved) = plan(c, &q, limit, offset)?;
    if !ambiguous.is_empty() {
        return Ok(SearchResponse {
            hits: Vec::new(),
            total_estimate: 0,
            next_offset: None,
            interpreted_as: q,
            ambiguous,
            unresolved,
        });
    }
    let bound: Vec<&dyn augmentagent_store::rusqlite::ToSql> = params
        .iter()
        .map(|p| p as &dyn augmentagent_store::rusqlite::ToSql)
        .collect();
    let mut stmt = c.prepare(&rows_sql)?;
    let rows: Vec<(i64, Hit)> = stmt
        .query_map(bound.as_slice(), row_to_hit)?
        .collect::<Result<_, _>>()?;
    let total: i64 = c.query_row(&count_sql, bound.as_slice(), |r| r.get(0))?;
    let mut snippet = c.prepare("SELECT substr(body, 1, ?2) FROM message_fts WHERE rowid = ?1")?;
    let hits: Vec<Hit> = rows
        .into_iter()
        .map(|(rowid, mut hit)| {
            hit.snippet = snippet
                .query_row(
                    augmentagent_store::rusqlite::params![rowid, SNIPPET_CHARS as i64],
                    |r| r.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .unwrap_or_default()
                .lines()
                .collect::<Vec<_>>()
                .join(" ");
            hit
        })
        .collect();
    let next_offset = (!q.latest && hits.len() == limit && (offset + limit) < total as usize)
        .then_some(offset + limit);
    Ok(SearchResponse {
        hits,
        total_estimate: total,
        next_offset,
        interpreted_as: q,
        ambiguous,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::drain;
    use augmentagent_store::{Email, Store};
    use std::time::Duration;

    const NOW: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

    fn p(q: &str) -> Query {
        parse_at(q, NOW).unwrap()
    }

    fn err(q: &str) -> ParseError {
        parse_at(q, NOW).unwrap_err()
    }

    fn w(s: &str) -> Term {
        Term::Word(s.into())
    }

    #[test]
    fn parses_every_operator() {
        let q = p("with:alex from:me in:discord,imessage is:dm after:2026-01-01 has:attachment sort:oldest budget");
        assert_eq!(q.with, ["alex"]);
        assert_eq!(q.from, ["me"]);
        assert_eq!(q.platforms, ["discord", "imessage"]);
        assert_eq!(q.kinds, ["dm"]);
        assert_eq!(q.after, Some(NOW));
        assert!(q.has_attachment);
        assert_eq!(q.sort, Some(Sort::Oldest));
        assert_eq!(q.include, [w("budget")]);
        let q = p("server:\"Acme HQ\" channel:general thread:t-1 is:latest");
        assert_eq!(q.server.as_deref(), Some("Acme HQ"));
        assert_eq!(q.channel.as_deref(), Some("general"));
        assert_eq!(q.thread.as_deref(), Some("t-1"));
        assert!(q.latest);
        assert_eq!(
            p("WITH:alex IN:Discord").platforms,
            ["discord"],
            "operators are case-insensitive"
        );
    }

    #[test]
    fn parses_text_terms_phrases_prefixes_and_negation() {
        let q = p("\"launch video\" invoic* -draft -in:gmail -is:group");
        assert_eq!(
            q.include,
            [
                Term::Phrase("launch video".into()),
                Term::Prefix("invoic".into())
            ]
        );
        assert_eq!(q.exclude, [w("draft")]);
        assert_eq!(q.not_platforms, ["gmail"]);
        assert_eq!(q.not_kinds, ["group"]);
        assert_eq!(p("with:alex with:sam").with, ["alex", "sam"], "repeatable");
    }

    #[test]
    fn relative_and_absolute_dates() {
        assert_eq!(p("after:7d").after, Some(NOW - 7 * 86_400_000));
        assert_eq!(p("after:2w").after, Some(NOW - 14 * 86_400_000));
        let on = p("on:2026-01-01");
        assert_eq!((on.after, on.before), (Some(NOW), Some(NOW + 86_400_000)));
        assert_eq!(
            p("after:2026-01-01T06:00:00Z").after,
            Some(NOW + 6 * 3_600_000)
        );
    }

    #[test]
    fn errors_name_the_token_and_list_valid_operators() {
        assert!(err("frm:alex").message.contains("unknown operator `frm:`"));
        assert!(err("frm:alex").message.contains("with"));
        assert_eq!(err("frm:alex").token, "frm:alex");
        assert!(err("is:whatever").message.contains("unknown `is:` value"));
        assert!(err("after:soon").message.contains("YYYY-MM-DD"));
        assert!(err("has:wings").message.contains("attachment"));
        assert!(err("sort:sideways").message.contains("relevance"));
        assert!(err("with:").message.contains("needs a value"));
        assert!(err("   ").message.contains("empty query"));
    }

    #[test]
    fn odd_input_parses_without_panic() {
        let long = "x".repeat(5000);
        for q in [
            "\"unbalanced",
            "-",
            "::",
            "a:b:c",
            "in:",
            "-\"x\"",
            "*",
            "-*",
            &long,
            "名前",
            "with:\"a b\"",
        ] {
            let _ = parse_at(q, NOW);
        }
        let mut seed: u64 = 99;
        let alphabet: Vec<char> = "ab:-\"* ,".chars().collect();
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 14) as usize;
            let s: String = (0..len)
                .map(|i| alphabet[(seed as usize >> i) % alphabet.len()])
                .collect();
            let _ = parse_at(&s, NOW);
        }
    }

    // ---- store-backed behaviour -------------------------------------------

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let wiki = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(wiki.path().join("people")).unwrap();
        let page = |slug: &str, front: &str, body: &str| {
            std::fs::write(
                wiki.path().join(format!("people/{slug}.md")),
                format!("---\nkind: person\n{front}---\n{body}"),
            )
            .unwrap()
        };
        page(
            "alex-kim",
            "identities:\n  email: [\"alex@example.com\"]\n  phone: [\"+14155550123\"]\n  discord: \"500\"\n",
            "",
        );
        page(
            "alex-stone",
            "identities:\n  email: [\"astone@example.org\"]\n",
            "",
        );
        page("sam-park", "identities:\n  discord: \"600\"\n", "");

        let put = |id: &str,
                   platform: &str,
                   from: &str,
                   thread: &str,
                   subject: &str,
                   body: &str,
                   kind: &str,
                   date: &str| {
            store
                .upsert_email(&Email {
                    message_id: id.into(),
                    thread_id: Some(thread.into()),
                    from: from.into(),
                    to: String::new(),
                    cc: String::new(),
                    attachments: vec![],
                    subject: subject.into(),
                    body: body.into(),
                    date: date.into(),
                    account_entity_id: Some("discord:900".into()),
                    platform: platform.into(),
                    kind: kind.into(),
                })
                .unwrap();
        };
        // Alex across three platforms, including the owner's own messages.
        put(
            "t1",
            "imessage",
            "+14155550123",
            "imessage:+14155550123",
            "iMessage: Alex Kim",
            "the budget doc is ready",
            "dm",
            "2025-12-01T10:00:00Z",
        );
        put(
            "t2",
            "imessage",
            "me",
            "imessage:+14155550123",
            "iMessage: Alex Kim",
            "thanks, reviewing the budget now",
            "dm",
            "2025-12-01T11:00:00Z",
        );
        put(
            "d1",
            "discord",
            "Alex <discord:500>",
            "chan-1",
            "Discord DM: Alex [Alex]",
            "sent you the launch video",
            "dm",
            "2025-12-20T09:00:00Z",
        );
        put(
            "d2",
            "discord",
            "me <discord:900>",
            "chan-1",
            "Discord DM: Alex [me]",
            "got it, editing tonight",
            "dm",
            "2025-12-20T09:30:00Z",
        );
        put(
            "g1",
            "gmail",
            "Alex Kim <alex@example.com>",
            "mail-1",
            "Budget review",
            "attached the numbers",
            "dm",
            "2025-12-22T12:00:00Z",
        );
        // A server channel and another person.
        put(
            "s1",
            "discord",
            "Sam <discord:600>",
            "chan-9",
            "Discord: Acme HQ #general [Sam]",
            "roadmap thoughts for the launch",
            "guild_channel",
            "2025-12-25T08:00:00Z",
        );
        put(
            "s2",
            "discord",
            "me <discord:900>",
            "chan-9",
            "Discord: Acme HQ #general [me]",
            "agreed, shipping friday",
            "guild_channel",
            "2025-12-26T08:00:00Z",
        );
        put(
            "x1",
            "whatsapp",
            "me",
            "whatsapp-history:5555@lid",
            "WhatsApp: Unknown [me]",
            "budget season again",
            "dm",
            "2025-11-01T08:00:00Z",
        );
        put(
            "a1",
            "imessage",
            "+14155550123",
            "imessage:+14155550123",
            "iMessage: Alex Kim",
            "photo",
            "dm",
            "2025-12-02T10:00:00Z",
        );
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE emails SET body = '[attachment: image/png plan.png https://cdn.example.com/plan.png]' WHERE messageId = 'a1'",
                    [],
                )
            })
            .unwrap();
        drain(&store, 100, Duration::ZERO).unwrap();
        crate::people::resolve_people(&store, wiki.path()).unwrap();
        (dir, wiki, store)
    }

    fn ids(s: &Store, q: &str) -> Vec<String> {
        s.with_conn(|c| {
            Ok(search(c, q, None, 0)
                .unwrap()
                .hits
                .into_iter()
                .map(|h| h.message_id)
                .collect::<Vec<_>>())
        })
        .unwrap()
    }

    fn resp(s: &Store, q: &str) -> SearchResponse {
        s.with_conn(|c| Ok(search(c, q, None, 0).unwrap())).unwrap()
    }

    #[test]
    fn with_person_spans_platforms_and_includes_owner_messages() {
        let (_d, _w, s) = fixture();
        let mut got = ids(&s, "with:\"alex kim\"");
        got.sort();
        assert_eq!(got, ["a1", "d1", "d2", "g1", "t1", "t2"]);
        assert!(!ids(&s, "with:\"alex kim\"").contains(&"s1".to_string()));
    }

    #[test]
    fn from_me_is_latest_returns_the_single_newest_message_to_that_person() {
        let (_d, _w, s) = fixture();
        let r = resp(&s, "with:\"alex kim\" from:me is:latest");
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].message_id, "d2");
        assert!(r.hits[0].from_me);
        assert_eq!(r.next_offset, None);
        assert_eq!(ids(&s, "to:\"alex kim\" is:latest"), ["d2"]);
    }

    #[test]
    fn platform_kind_server_channel_and_thread_filters() {
        let (_d, _w, s) = fixture();
        assert_eq!(ids(&s, "with:\"alex kim\" in:gmail"), ["g1"]);
        let mut dm = ids(&s, "is:dm in:discord");
        dm.sort();
        assert_eq!(dm, ["d1", "d2"]);
        let mut ch = ids(&s, "server:acme channel:general");
        ch.sort();
        assert_eq!(ch, ["s1", "s2"]);
        assert_eq!(
            ids(&s, "server:\"acme hq\" channel:\"#general\" is:latest"),
            ["s2"]
        );
        let mut th = ids(&s, "thread:chan-1");
        th.sort();
        assert_eq!(th, ["d1", "d2"]);
        assert_eq!(ids(&s, "is:channel -in:gmail sort:oldest"), ["s1", "s2"]);
    }

    #[test]
    fn date_bounds_and_attachment_and_negation() {
        let (_d, _w, s) = fixture();
        assert_eq!(
            ids(&s, "budget after:2025-12-01 before:2025-12-02"),
            ["t1", "t2"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
        );
        assert_eq!(ids(&s, "on:2025-12-20 sort:oldest"), ["d1", "d2"]);
        assert_eq!(ids(&s, "has:attachment"), ["a1"]);
        let mut no_budget = ids(&s, "launch -video");
        no_budget.sort();
        assert_eq!(no_budget, ["s1"]);
    }

    #[test]
    fn text_search_is_ranked_and_pagination_is_stable() {
        let (_d, _w, s) = fixture();
        let all = ids(&s, "budget");
        assert!(
            all.contains(&"g1".to_string()),
            "title hit ranks in: {all:?}"
        );
        let page1 = s
            .with_conn(|c| Ok(search(c, "budget sort:newest", Some(2), 0).unwrap()))
            .unwrap();
        assert_eq!(page1.hits.len(), 2);
        assert_eq!(page1.next_offset, Some(2));
        let page2 = s
            .with_conn(|c| Ok(search(c, "budget sort:newest", Some(2), 2).unwrap()))
            .unwrap();
        let mut seen: Vec<String> = page1
            .hits
            .iter()
            .chain(page2.hits.iter())
            .map(|h| h.message_id.clone())
            .collect();
        let before = seen.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), before.len(), "no duplicates across pages");
        assert_eq!(page1.total_estimate, all.len() as i64);
    }

    #[test]
    fn ambiguous_person_returns_candidates_and_no_rows() {
        let (_d, _w, s) = fixture();
        let r = resp(&s, "with:alex");
        assert!(r.hits.is_empty());
        assert_eq!(r.ambiguous.len(), 1);
        assert_eq!(r.ambiguous[0].query, "alex");
        let mut keys: Vec<&str> = r.ambiguous[0]
            .candidates
            .iter()
            .map(|c| c.key.as_str())
            .collect();
        keys.sort();
        assert_eq!(keys, ["alex-kim", "alex-stone"]);
    }

    #[test]
    fn unknown_person_falls_back_to_raw_handle() {
        let (_d, _w, s) = fixture();
        let r = resp(&s, "with:+14155550123");
        assert_eq!(
            r.unresolved,
            Vec::<String>::new(),
            "this number is on a page"
        );
        assert!(!r.hits.is_empty());
        let r = resp(&s, "with:nobody@example.net");
        assert_eq!(r.unresolved, ["nobody@example.net"]);
        assert!(r.hits.is_empty());
        assert!(r.ambiguous.is_empty());
    }

    #[test]
    fn hits_carry_person_conversation_and_snippet() {
        let (_d, _w, s) = fixture();
        let r = resp(&s, "thread:chan-1 sort:oldest");
        let first = &r.hits[0];
        assert_eq!(first.person.as_deref(), Some("alex-kim"));
        assert_eq!(first.conversation_title.as_deref(), Some("Alex"));
        assert_eq!(first.platform, "discord");
        assert!(first.snippet.contains("launch video"));
        assert!(first.timestamp.starts_with("2025-12-20"));
        assert_eq!(r.interpreted_as.thread.as_deref(), Some("chan-1"));
    }

    #[test]
    fn injected_operators_in_values_cannot_change_the_sql() {
        let (_d, _w, s) = fixture();
        let evil = [
            "with:\"x' OR 1=1 --\"",
            "thread:\"'; DROP TABLE emails; --\"",
            "channel:\"%\"",
            "server:\"_\"",
            "\"x\\\" OR body MATCH \\\"budget\"",
            "in:\"gmail') UNION SELECT 1 --\"",
        ];
        for q in evil {
            let r = resp(&s, q);
            assert!(r.hits.is_empty(), "{q} returned rows");
        }
        let alive: i64 = s
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(alive, 9, "no statement escaped its parameters");
    }

    #[test]
    fn limit_is_clamped_and_plans_avoid_scanning_emails() {
        let (_d, _w, s) = fixture();
        let r = s
            .with_conn(|c| Ok(search(c, "budget", Some(9999), 0).unwrap()))
            .unwrap();
        assert!(r.hits.len() <= MAX_LIMIT);
        for q in [
            "budget",
            "with:\"alex kim\"",
            "is:dm in:discord sort:newest",
            "server:acme",
        ] {
            let plan_rows: Vec<String> = s
                .with_conn(|c| {
                    let query = parse_at(q, NOW).unwrap();
                    let (sql, _, params, _, _) = plan(c, &query, 20, 0)?;
                    let bound: Vec<&dyn augmentagent_store::rusqlite::ToSql> = params
                        .iter()
                        .map(|p| p as &dyn augmentagent_store::rusqlite::ToSql)
                        .collect();
                    let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let rows = stmt.query_map(bound.as_slice(), |r| r.get::<_, String>(3))?;
                    rows.collect()
                })
                .unwrap();
            let joined = plan_rows.join(" | ");
            assert!(!joined.contains("SCAN emails"), "{q}: {joined}");
        }
    }
}
