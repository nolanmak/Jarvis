//! FTS5-backed cross-session memory MCP server.
//!
//! Exposes three tools over the MCP JSON-RPC stdio protocol so Claude Code
//! can recall prior context across cycles:
//!
//! - `memory_search(query, limit?)` — FTS5 full-text search over the
//!   `memory` table created by [`augmentagent_store::Store`]'s migration
//!   (#111). Returns hits ranked by FTS5's built-in BM25 score.
//! - `memory_write(surface, subject, body, tags?)` — append a memory entry.
//!   Surface is the channel that produced the memory (`email`, `slack`,
//!   `discord`, `ask`, `digest`, `other`); `tags` is a comma-joined list
//!   surfaced both literally in the row and indexed by FTS5.
//! - `memory_recent(surface?, limit?)` — chronological recall when search
//!   isn't the right shape. Optional surface filter narrows the result set.
//!
//! Wire layout: [`Server`] owns the SQLite connection + handler logic.
//! [`serve_stdio`] is the IO shell — it reads newline-delimited JSON-RPC
//! requests from stdin, hands them to [`Server::dispatch`], and writes
//! responses to stdout. This split keeps the protocol parsing testable
//! without spawning an actual stdio loop.

use std::path::Path;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One memory row as stored on disk + returned by `memory_search` /
/// `memory_recent`. `score` is FTS5's BM25 score for searches (lower =
/// better) or `None` for chronological reads where rank isn't meaningful.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRow {
    pub id: String,
    pub created_at_ms: i64,
    pub surface: String,
    pub subject: String,
    pub body: String,
    pub tags: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// One hit from `search_conversation_history`. Snippet is the first
/// `SNIPPET_MAX_CHARS` chars of the message body so the agent gets enough
/// signal to decide whether to re-fetch the full message via `rowid`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationHit {
    /// SQLite rowid of the underlying row. Stable for the lifetime of the
    /// row; lets a follow-up tool fetch the full content if the snippet
    /// isn't enough.
    pub rowid: i64,
    /// Epoch ms. For inbound rows this is `emails.firstSeenAt`; for agent
    /// drafts it's `actions.createdAt`.
    pub timestamp_ms: i64,
    /// `discord`, `email`, `slack`, `linkedin`, etc. — i.e. the channel
    /// the message came in / went out on.
    pub channel: String,
    /// `user` for inbound messages, `agent` for drafts the agent produced.
    pub role: String,
    /// Platform-native thread id: the LinkedIn conversation urn, the
    /// SocialAPI.ai conversation id, the Gmail threadId, etc.
    ///
    /// This is the field the card-raising verbs need — `linkedin dm
    /// --conversation-urn`, `socialapi dm --conversation-id` — and it was
    /// simply not selected. The agent could read a DM's text out of this tool
    /// and still be unable to reply to it, because the id was sitting in the
    /// same row it had just read. `None` for rows with no thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Platform-native message id. `socialapi comment` needs it as the parent
    /// comment id, and it is how a caller re-finds the exact message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// First [`SNIPPET_MAX_CHARS`] chars of the body, with subject prefixed
    /// when present so a single line read tells the agent what it's looking
    /// at.
    pub snippet: String,
}

/// Maximum snippet length returned per conversation hit. Picked to keep
/// 100 hits well under a 64KB tool result while still showing enough
/// context for the agent to pick a candidate.
pub const SNIPPET_MAX_CHARS: usize = 300;

/// Pagination cap for any single call. Matches Claude Code's typical
/// per-tool reply budget so a runaway `memory_search` doesn't blow out
/// the context window.
pub const MAX_LIMIT: usize = 100;

/// Default limit when the caller doesn't supply one.
pub const DEFAULT_LIMIT: usize = 10;

/// Default limit for `search_conversation_history`. Higher than
/// [`DEFAULT_LIMIT`] because conversation snippets are shorter than
/// memory rows so 20 fits comfortably in a single tool result.
pub const DEFAULT_CONVO_LIMIT: usize = 20;

/// In-process server. Owns the SQLite connection. Cloning is intentionally
/// not implemented — the MCP stdio loop is single-threaded and we want a
/// single writer to avoid SQLite write contention in tests.
pub struct Server {
    conn: Connection,
}

impl Server {
    /// Open a server backed by the database at `path`. Runs the
    /// [`augmentagent_store::Store`] migration first so a fresh box can
    /// boot this binary without the main daemon having created the file.
    /// Idempotent — calling on an already-migrated db is a no-op.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        // Side-effect of dropping the Store: runs the full migration,
        // including the `memory` + `memory_fts` schema (#111).
        let _ = augmentagent_store::Store::open(path.as_ref())
            .with_context(|| format!("Store::open({})", path.as_ref().display()))?;
        // Open our own raw connection so we don't have to layer the FTS5
        // queries through the high-level Store API (FTS5 isn't part of
        // the existing store surface and there's no reason to drag every
        // memory query through there).
        let conn = Connection::open(path.as_ref())
            .with_context(|| format!("Connection::open({})", path.as_ref().display()))?;
        Ok(Self { conn })
    }


    /// Insert a memory row. Returns the assigned id. `surface` is trimmed;
    /// `subject`/`body` are stored as-supplied (whitespace can matter in
    /// retrieval prompts). `tags` is normalized to a comma-joined string
    /// regardless of input shape: callers can pass `""`, `"a,b"`, or skip
    /// the param entirely.
    pub fn write(
        &self,
        surface: &str,
        subject: &str,
        body: &str,
        tags: Option<&str>,
    ) -> anyhow::Result<String> {
        let surface = surface.trim();
        if surface.is_empty() {
            anyhow::bail!("memory_write: surface must not be empty");
        }
        if subject.trim().is_empty() {
            anyhow::bail!("memory_write: subject must not be empty");
        }
        let id = uuid::Uuid::new_v4().to_string();
        // Wall-clock ms since epoch. Matches the `_ms` suffix convention
        // every other table in `store.rs` uses (`created_at_ms`).
        let now_ms = current_ms();
        let tags_normalized = tags.map(|t| t.trim()).unwrap_or("");
        self.conn
            .execute(
                "INSERT INTO memory (id, created_at_ms, surface, subject, body, tags) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, now_ms, surface, subject, body, tags_normalized],
            )
            .context("insert memory row")?;
        Ok(id)
    }

    /// Search via FTS5. `query` is passed verbatim to FTS5's MATCH
    /// operator (callers can use prefix queries with `*`, boolean ops, etc).
    /// `limit` is clamped to [`MAX_LIMIT`]; `None` uses [`DEFAULT_LIMIT`].
    pub fn search(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<MemoryRow>> {
        let query = query.trim();
        if query.is_empty() {
            anyhow::bail!("memory_search: query must not be empty");
        }
        let limit = clamp_limit(limit);
        // Order by FTS5's built-in rank function (BM25). Lower rank =
        // better match in FTS5's scoring (negative when normalized).
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.id, m.created_at_ms, m.surface, m.subject, m.body, m.tags, \
                        bm25(memory_fts) AS score \
                 FROM memory_fts \
                 JOIN memory m ON m.rowid = memory_fts.rowid \
                 WHERE memory_fts MATCH ?1 \
                 ORDER BY score ASC \
                 LIMIT ?2",
            )
            .context("prepare memory_search query")?;
        let rows = stmt
            .query_map(params![query, limit as i64], row_to_memory_scored)
            .context("execute memory_search query")?
            .collect::<Result<Vec<_>, _>>()
            .context("collect memory_search results")?;
        Ok(rows)
    }

    /// Chronological recall. `surface = None` means "any surface"; `Some(s)`
    /// filters to that single surface. `limit` clamps like [`Self::search`].
    pub fn recent(
        &self,
        surface: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<MemoryRow>> {
        let limit = clamp_limit(limit);
        let rows = match surface {
            Some(s) if !s.trim().is_empty() => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, created_at_ms, surface, subject, body, tags \
                     FROM memory \
                     WHERE surface = ?1 \
                     ORDER BY created_at_ms DESC \
                     LIMIT ?2",
                )?;
                let rows: Result<Vec<_>, _> = stmt
                    .query_map(params![s.trim(), limit as i64], row_to_memory_unscored)?
                    .collect();
                rows?
            }
            _ => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, created_at_ms, surface, subject, body, tags \
                     FROM memory \
                     ORDER BY created_at_ms DESC \
                     LIMIT ?1",
                )?;
                let rows: Result<Vec<_>, _> = stmt
                    .query_map(params![limit as i64], row_to_memory_unscored)?
                    .collect();
                rows?
            }
        };
        Ok(rows)
    }

    /// Search prior conversation history across all channels (#252).
    ///
    /// Reads two underlying tables the daemon already populates:
    /// - `emails` — every inbound DM/post the channel layer ingests,
    ///   regardless of `platform` (gmail, discord, slack, …). These
    ///   become `role = "user"` hits.
    /// - `actions.draftBody` — the agent's outbound drafts, joined back
    ///   to `emails` so the hit carries the same channel as the message
    ///   it replied to. These become `role = "agent"` hits.
    ///
    /// Filtering is intentionally simple: case-insensitive `LIKE %kw%`
    /// over subject+body, optional `since`/`until` bounds (epoch ms), and
    /// optional channel (matched against `emails.platform`). All
    /// parameters are bound — no string concat.
    ///
    /// `limit` clamps to [`MAX_LIMIT`]. At least one of `keyword`,
    /// `since_ms`, `until_ms` must be set; otherwise we refuse so the
    /// agent can't accidentally pull the entire log into context.
    pub fn search_conversation_history(
        &self,
        keyword: Option<&str>,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        channel: Option<&str>,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<ConversationHit>> {
        let keyword = keyword.map(str::trim).filter(|s| !s.is_empty());
        let channel = channel.map(str::trim).filter(|s| !s.is_empty());
        if keyword.is_none() && since_ms.is_none() && until_ms.is_none() {
            anyhow::bail!(
                "search_conversation_history: at least one of keyword, since, until is required"
            );
        }
        let limit = clamp_limit_with_default(limit, DEFAULT_CONVO_LIMIT);

        // LIKE pattern: case-insensitivity handled by lowercasing both sides.
        // We bind the pattern as `%kw%` once and reuse it for subject + body.
        let like_pattern = keyword.map(|k| format!("%{}%", k.to_lowercase()));

        // Two SELECTs UNION ALL'd then sorted by timestamp DESC + LIMIT.
        // Each leg uses the same five bind slots so the prepared statement
        // is uniform; channel/keyword filters short-circuit via `?N IS NULL`.
        // Order of binds (per leg): keyword_like, keyword_like, since_ms,
        // until_ms, channel.
        let sql = "\
            SELECT rowid, timestamp_ms, channel, role, snippet, thread_id, message_id FROM ( \
                SELECT \
                    e.rowid AS rowid, \
                    e.firstSeenAt AS timestamp_ms, \
                    e.platform AS channel, \
                    'user' AS role, \
                    e.threadId AS thread_id, \
                    e.messageId AS message_id, \
                    COALESCE(NULLIF(e.subject, '') || ': ', '') || COALESCE(e.body, '') AS snippet \
                FROM emails e \
                WHERE (?1 IS NULL \
                       OR LOWER(COALESCE(e.body, '')) LIKE ?1 \
                       OR LOWER(COALESCE(e.subject, '')) LIKE ?2) \
                  AND (?3 IS NULL OR e.firstSeenAt >= ?3) \
                  AND (?4 IS NULL OR e.firstSeenAt <= ?4) \
                  AND (?5 IS NULL OR e.platform = ?5) \
                UNION ALL \
                SELECT \
                    a.rowid AS rowid, \
                    a.createdAt AS timestamp_ms, \
                    COALESCE(e2.platform, 'unknown') AS channel, \
                    'agent' AS role, \
                    a.threadId AS thread_id, \
                    a.messageId AS message_id, \
                    COALESCE(NULLIF(a.subject, '') || ': ', '') || COALESCE(a.draftBody, '') AS snippet \
                FROM actions a \
                LEFT JOIN emails e2 ON e2.messageId = a.messageId \
                WHERE a.draftBody IS NOT NULL AND a.draftBody <> '' \
                  AND (?1 IS NULL \
                       OR LOWER(a.draftBody) LIKE ?1 \
                       OR LOWER(COALESCE(a.subject, '')) LIKE ?2) \
                  AND (?3 IS NULL OR a.createdAt >= ?3) \
                  AND (?4 IS NULL OR a.createdAt <= ?4) \
                  AND (?5 IS NULL OR COALESCE(e2.platform, 'unknown') = ?5) \
            ) \
            ORDER BY timestamp_ms DESC \
            LIMIT ?6";

        let mut stmt = self
            .conn
            .prepare(sql)
            .context("prepare search_conversation_history query")?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    like_pattern,
                    like_pattern,
                    since_ms,
                    until_ms,
                    channel,
                    limit as i64,
                ],
                |row| {
                    let raw_snippet: String = row.get(4)?;
                    // Blank-to-None so an empty column doesn't look like a
                    // usable id to a caller about to pass it to `--conversation-urn`.
                    let opt = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
                    Ok(ConversationHit {
                        rowid: row.get(0)?,
                        timestamp_ms: row.get(1)?,
                        channel: row.get(2)?,
                        role: row.get(3)?,
                        snippet: truncate_snippet(&raw_snippet),
                        thread_id: opt(row.get(5)?),
                        message_id: opt(row.get(6)?),
                    })
                },
            )
            .context("execute search_conversation_history query")?
            .collect::<Result<Vec<_>, _>>()
            .context("collect search_conversation_history results")?;
        Ok(rows)
    }

    /// Single-row lookup by id. Useful for tests + future "follow-up on
    /// memory <id>" tool surfaces.
    pub fn get(&self, id: &str) -> anyhow::Result<Option<MemoryRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, created_at_ms, surface, subject, body, tags \
                 FROM memory WHERE id = ?1",
                params![id],
                row_to_memory_unscored,
            )
            .optional()?;
        Ok(row)
    }

    /// Dispatch a single MCP JSON-RPC request and produce a response.
    ///
    /// Recognised methods (per the MCP spec):
    /// - `initialize` — protocol handshake; returns server metadata.
    /// - `tools/list` — enumerate exposed tools.
    /// - `tools/call` — invoke a tool. `params.name` selects the tool;
    ///   `params.arguments` is a JSON object matching the tool's schema.
    ///
    /// Returns a JSON value ready for serialization. Errors are mapped to
    /// JSON-RPC error objects (no panics across the IO boundary).
    pub fn dispatch(&self, req: &McpRequest) -> Value {
        match req.method.as_str() {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "augmentagent-mcp-memory",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "result": { "tools": tool_descriptors() }
            }),
            "tools/call" => self.handle_tool_call(req),
            other => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "error": {
                    "code": -32601,
                    "message": format!("method not found: {other}"),
                }
            }),
        }
    }

    fn handle_tool_call(&self, req: &McpRequest) -> Value {
        let params = req.params.as_ref().unwrap_or(&Value::Null);
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        match name {
            "memory_write" => self.tool_memory_write(req.id.clone(), &args),
            "memory_search" => self.tool_memory_search(req.id.clone(), &args),
            "memory_recent" => self.tool_memory_recent(req.id.clone(), &args),
            "search_conversation_history" => {
                self.tool_search_conversation_history(req.id.clone(), &args)
            }
            other => json!({
                "jsonrpc": "2.0",
                "id": req.id,
                "error": {
                    "code": -32602,
                    "message": format!("unknown tool: {other}"),
                }
            }),
        }
    }

    fn tool_memory_write(&self, id: Value, args: &Value) -> Value {
        let surface = args.get("surface").and_then(Value::as_str).unwrap_or("");
        let subject = args.get("subject").and_then(Value::as_str).unwrap_or("");
        let body = args.get("body").and_then(Value::as_str).unwrap_or("");
        let tags = args.get("tags").and_then(Value::as_str);
        match self.write(surface, subject, body, tags) {
            Ok(new_id) => tool_text_result(id, &format!("wrote memory {new_id}")),
            Err(e) => tool_error(id, format!("{e}")),
        }
    }

    fn tool_memory_search(&self, id: Value, args: &Value) -> Value {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        match self.search(query, limit) {
            Ok(rows) => tool_json_result(id, &json!({ "hits": rows })),
            Err(e) => tool_error(id, format!("{e}")),
        }
    }

    fn tool_memory_recent(&self, id: Value, args: &Value) -> Value {
        let surface = args.get("surface").and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        match self.recent(surface, limit) {
            Ok(rows) => tool_json_result(id, &json!({ "hits": rows })),
            Err(e) => tool_error(id, format!("{e}")),
        }
    }

    fn tool_search_conversation_history(&self, id: Value, args: &Value) -> Value {
        let keyword = args.get("keyword").and_then(Value::as_str);
        let channel = args.get("channel").and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let since_ms = match args.get("since").and_then(Value::as_str) {
            Some(s) => match parse_date_to_ms(s, /* end_of_day = */ false) {
                Ok(ms) => Some(ms),
                Err(e) => return tool_error(id, format!("invalid `since`: {e}")),
            },
            None => None,
        };
        let until_ms = match args.get("until").and_then(Value::as_str) {
            Some(s) => match parse_date_to_ms(s, /* end_of_day = */ true) {
                Ok(ms) => Some(ms),
                Err(e) => return tool_error(id, format!("invalid `until`: {e}")),
            },
            None => None,
        };
        match self.search_conversation_history(keyword, since_ms, until_ms, channel, limit) {
            Ok(rows) => tool_json_result(id, &json!({ "hits": rows })),
            Err(e) => tool_error(id, format!("{e}")),
        }
    }
}

/// One incoming JSON-RPC request. Fields match the MCP spec literally; we
/// keep `params` as a free-form `Value` because each method's schema is
/// independent and we don't want a closed enum here.
#[derive(Debug, Deserialize)]
pub struct McpRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// Tool descriptor list returned by `tools/list`. Stable JSON shape;
/// Claude Code uses the `inputSchema` to build the tool-use prompt.
fn tool_descriptors() -> Value {
    json!([
        {
            "name": "memory_write",
            "description": "Persist a memory entry the agent can recall in future cycles.",
            "inputSchema": {
                "type": "object",
                "required": ["surface", "subject", "body"],
                "properties": {
                    "surface": { "type": "string", "description": "channel that produced this memory (email/slack/discord/ask/digest/other)" },
                    "subject": { "type": "string", "description": "short headline; what the memory is about" },
                    "body":    { "type": "string", "description": "free-form details" },
                    "tags":    { "type": "string", "description": "comma-separated tags (optional)" }
                }
            }
        },
        {
            "name": "memory_search",
            "description": "Full-text search prior memories (FTS5 BM25 ranking).",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "FTS5 MATCH expression" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 }
                }
            }
        },
        {
            "name": "memory_recent",
            "description": "Recent memories in reverse-chronological order; optional surface filter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "surface": { "type": "string", "description": "filter to a single surface; omit for all" },
                    "limit":   { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 }
                }
            }
        },
        {
            "name": "search_conversation_history",
            "description": "Search prior conversation history (inbound messages + agent drafts) across all channels. At least one of `keyword`, `since`, `until` is required.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keyword": { "type": "string", "description": "case-insensitive substring matched against message subject/body" },
                    "since":   { "type": "string", "description": "lower bound on timestamp; ISO 8601 or YYYY-MM-DD" },
                    "until":   { "type": "string", "description": "upper bound on timestamp; ISO 8601 or YYYY-MM-DD" },
                    "channel": { "type": "string", "description": "restrict to one platform (e.g. discord, gmail, slack, linkedin)" },
                    "limit":   { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
                }
            }
        }
    ])
}

fn tool_text_result(id: Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }]
        }
    })
}

fn tool_json_result(id: Value, payload: &Value) -> Value {
    // MCP `tools/call` results carry a `content` array. We serialize the
    // hit set as a single text block of pretty JSON so Claude Code's tool-
    // result renderer surfaces it readably without us needing a structured
    // content type.
    let text = serde_json::to_string_pretty(payload).unwrap_or_default();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": text }]
        }
    })
}

fn tool_error(id: Value, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32000,
            "message": message,
        }
    })
}

fn row_to_memory_unscored(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: row.get(0)?,
        created_at_ms: row.get(1)?,
        surface: row.get(2)?,
        subject: row.get(3)?,
        body: row.get(4)?,
        tags: row.get(5)?,
        score: None,
    })
}

fn row_to_memory_scored(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: row.get(0)?,
        created_at_ms: row.get(1)?,
        surface: row.get(2)?,
        subject: row.get(3)?,
        body: row.get(4)?,
        tags: row.get(5)?,
        score: row.get::<_, Option<f64>>(6)?,
    })
}

fn clamp_limit(limit: Option<usize>) -> usize {
    clamp_limit_with_default(limit, DEFAULT_LIMIT)
}

fn clamp_limit_with_default(limit: Option<usize>, default: usize) -> usize {
    match limit {
        None => default,
        Some(0) => default,
        Some(n) if n > MAX_LIMIT => MAX_LIMIT,
        Some(n) => n,
    }
}

/// Truncate a snippet to [`SNIPPET_MAX_CHARS`] on a char boundary, adding
/// a `…` marker when truncation occurred so the agent can tell the
/// snippet was cut off and decide whether to fetch the full row.
fn truncate_snippet(s: &str) -> String {
    if s.chars().count() <= SNIPPET_MAX_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(SNIPPET_MAX_CHARS).collect();
    out.push('…');
    out
}

/// Parse a user-supplied date into epoch ms. Accepts:
/// - `YYYY-MM-DD` — interpreted as 00:00:00 UTC (or 23:59:59.999 UTC if
///   `end_of_day`).
/// - ISO 8601 / RFC 3339 timestamps (e.g. `2025-05-12T14:00:00Z`).
fn parse_date_to_ms(s: &str, end_of_day: bool) -> anyhow::Result<i64> {
    use chrono::{NaiveDate, TimeZone, Utc};
    let trimmed = s.trim();
    // Date-only path: anchor to start- or end-of-day in UTC.
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = if end_of_day {
            date.and_hms_milli_opt(23, 59, 59, 999)
        } else {
            date.and_hms_opt(0, 0, 0)
        }
        .ok_or_else(|| anyhow::anyhow!("invalid date components: {trimmed}"))?;
        return Ok(Utc.from_utc_datetime(&dt).timestamp_millis());
    }
    // Full ISO 8601 / RFC 3339 path.
    let dt = chrono::DateTime::parse_from_rfc3339(trimmed)
        .map_err(|e| anyhow::anyhow!("expected YYYY-MM-DD or ISO 8601, got `{trimmed}`: {e}"))?;
    Ok(dt.timestamp_millis())
}

fn current_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Stdio JSON-RPC loop. Each line on stdin is one request; one line on
/// stdout per response. Lines without parseable JSON-RPC are reported as
/// parse-error responses (the spec's `-32700`) rather than terminating
/// the loop, so a single malformed message doesn't kill the server.
///
/// Returns `Ok(())` only on graceful stdin EOF.
pub fn serve_stdio(server: Server) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.context("read stdin")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<McpRequest>(trimmed) {
            Ok(req) => server.dispatch(&req),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {
                    "code": -32700,
                    "message": format!("parse error: {e}"),
                }
            }),
        };
        let serialized = serde_json::to_string(&response).context("serialize response")?;
        writeln!(out, "{serialized}").context("write stdout")?;
        out.flush().context("flush stdout")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Test fixture: tempdir + server. The tempdir lives as long as the
    /// fixture so the on-disk db is cleaned up when the test ends. Deref'd
    /// to `Server` so test bodies read like `s.write(...)`.
    pub(crate) struct Fixture {
        server: Server,
        _tmp: TempDir,
    }

    impl std::ops::Deref for Fixture {
        type Target = Server;
        fn deref(&self) -> &Server {
            &self.server
        }
    }

    pub(crate) fn new_server() -> Fixture {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("memory.db");
        let server = Server::open(&path).expect("open server");
        Fixture { server, _tmp: tmp }
    }

    #[test]
    fn write_then_get_round_trips() {
        let s = new_server();
        let id = s
            .write("email", "subj", "body text", Some("tag1,tag2"))
            .expect("write");
        let row = s.get(&id).expect("get").expect("present");
        assert_eq!(row.surface, "email");
        assert_eq!(row.subject, "subj");
        assert_eq!(row.body, "body text");
        assert_eq!(row.tags, "tag1,tag2");
        assert!(row.created_at_ms > 0);
    }

    #[test]
    fn write_rejects_empty_surface_or_subject() {
        let s = new_server();
        let err = s.write("", "subj", "b", None).unwrap_err();
        assert!(format!("{err}").contains("surface"));
        let err = s.write("email", "  ", "b", None).unwrap_err();
        assert!(format!("{err}").contains("subject"));
    }

    #[test]
    fn search_finds_token_match() {
        let s = new_server();
        s.write("email", "weekly groceries", "ordered eggs and milk", None).unwrap();
        s.write("digest", "monthly summary", "metrics on growth", None).unwrap();
        let hits = s.search("groceries", None).expect("search");
        assert_eq!(hits.len(), 1, "exactly one row matches");
        assert_eq!(hits[0].subject, "weekly groceries");
        assert!(hits[0].score.is_some(), "FTS5 returns a score");
    }

    #[test]
    fn search_respects_limit_clamp() {
        let s = new_server();
        for i in 0..5 {
            s.write("email", &format!("foo {i}"), "body foo", None).unwrap();
        }
        // Limit > MAX clamps down to MAX (we only have 5 rows, so all return).
        let hits = s.search("foo", Some(9999)).expect("search");
        assert_eq!(hits.len(), 5);
        // limit=2 caps explicit pagination.
        let hits = s.search("foo", Some(2)).expect("search");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_empty_query_is_rejected() {
        let s = new_server();
        let err = s.search("   ", None).unwrap_err();
        assert!(format!("{err}").contains("query"));
    }

    #[test]
    fn recent_returns_descending_order() {
        let s = new_server();
        let _ = s.write("email", "first", "b1", None).unwrap();
        // Sleep is too flaky in unit tests; just rely on monotonic insert
        // order — sqlite rowid is monotonic so created_at_ms ties will
        // resolve by rowid even at sub-ms timings.
        let _ = s.write("email", "second", "b2", None).unwrap();
        let _ = s.write("email", "third", "b3", None).unwrap();
        let rows = s.recent(None, Some(10)).expect("recent");
        assert_eq!(rows.len(), 3);
        // Newest-first: "third" before "first".
        let subjects: Vec<_> = rows.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects[0], "third");
    }

    #[test]
    fn recent_filters_by_surface() {
        let s = new_server();
        s.write("email", "e1", "b", None).unwrap();
        s.write("slack", "s1", "b", None).unwrap();
        s.write("email", "e2", "b", None).unwrap();
        let rows = s.recent(Some("email"), None).expect("recent email");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.surface == "email"));
    }

    #[test]
    fn dispatch_initialize_returns_handshake() {
        let s = new_server();
        let req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(1),
            method: "initialize".into(),
            params: None,
        };
        let resp = s.dispatch(&req);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "augmentagent-mcp-memory");
        assert!(resp["result"]["protocolVersion"].is_string());
    }

    #[test]
    fn dispatch_tools_list_enumerates_all_tools() {
        let s = new_server();
        let req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(2),
            method: "tools/list".into(),
            params: None,
        };
        let resp = s.dispatch(&req);
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let names: Vec<_> = tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert!(names.contains(&"memory_write"));
        assert!(names.contains(&"memory_search"));
        assert!(names.contains(&"memory_recent"));
        assert!(names.contains(&"search_conversation_history"));
    }

    #[test]
    fn dispatch_tool_write_and_search_round_trip() {
        let s = new_server();
        // Write via the tool surface (not the direct API) so we exercise
        // arg-parsing and result-shape together.
        let write_req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(3),
            method: "tools/call".into(),
            params: Some(json!({
                "name": "memory_write",
                "arguments": {
                    "surface": "ask",
                    "subject": "what year is rust 2",
                    "body": "user asked about Rust 2 plans",
                    "tags": "rust,history"
                }
            })),
        };
        let resp = s.dispatch(&write_req);
        // Result is a content block with "wrote memory <id>".
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("write text");
        assert!(text.starts_with("wrote memory "));
        // Search for "rust" should find the row.
        let search_req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(4),
            method: "tools/call".into(),
            params: Some(json!({
                "name": "memory_search",
                "arguments": { "query": "rust" }
            })),
        };
        let resp = s.dispatch(&search_req);
        let body = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("search text");
        assert!(body.contains("rust 2"), "expected hit in body: {body}");
    }

    #[test]
    fn dispatch_unknown_method_returns_jsonrpc_error() {
        let s = new_server();
        let req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(99),
            method: "totally/made/up".into(),
            params: None,
        };
        let resp = s.dispatch(&req);
        assert_eq!(resp["error"]["code"], -32601);
    }

    /// Direct insert helpers — the conversation tables are owned by
    /// `augmentagent-store` and there isn't a write-path API exposed for
    /// `actions` we can reuse, so tests seed them by raw SQL via the
    /// server's connection.
    pub(crate) fn insert_email(
        s: &Server,
        message_id: &str,
        platform: &str,
        subject: &str,
        body: &str,
        first_seen_at: i64,
    ) {
        insert_email_threaded(s, message_id, platform, subject, body, first_seen_at, None)
    }

    /// Same, with an explicit thread id — the LinkedIn conversation urn /
    /// SocialAPI conversation id a reply has to target.
    pub(crate) fn insert_email_threaded(
        s: &Server,
        message_id: &str,
        platform: &str,
        subject: &str,
        body: &str,
        first_seen_at: i64,
        thread_id: Option<&str>,
    ) {
        s.conn
            .execute(
                "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, accountEntityId, firstSeenAt, platform, kind) \
                 VALUES (?1, ?7, ?2, ?3, ?4, NULL, NULL, ?5, ?6, 'dm')",
                rusqlite::params![
                    message_id,
                    "sender@example.com",
                    subject,
                    body,
                    first_seen_at,
                    platform,
                    thread_id
                ],
            )
            .expect("seed email");
    }

    fn insert_action(
        s: &Server,
        id: &str,
        message_id: &str,
        subject: &str,
        draft_body: &str,
        created_at: i64,
    ) {
        s.conn
            .execute(
                "INSERT INTO actions (id, messageId, threadId, fromEmail, subject, originalBody, draftBody, status, errorMessage, createdAt, updatedAt) \
                 VALUES (?1, ?2, NULL, ?3, ?4, NULL, ?5, 'sent', NULL, ?6, ?6)",
                rusqlite::params![id, message_id, "sender@example.com", subject, draft_body, created_at],
            )
            .expect("seed action");
    }

    #[test]
    fn search_conversation_history_requires_a_filter() {
        let s = new_server();
        let err = s
            .search_conversation_history(None, None, None, None, None)
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("keyword") && msg.contains("since") && msg.contains("until"),
            "got: {msg}"
        );
    }

    #[test]
    fn search_conversation_history_finds_inbound_by_keyword() {
        let s = new_server();
        insert_email(&s, "m1", "discord", "rust talk", "we discussed rust 2 plans", 1_700_000_000_000);
        insert_email(&s, "m2", "gmail", "lunch", "want to grab lunch tomorrow", 1_700_000_100_000);
        let hits = s
            .search_conversation_history(Some("rust"), None, None, None, None)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].channel, "discord");
        assert_eq!(hits[0].role, "user");
        assert!(hits[0].snippet.contains("rust"), "snippet: {}", hits[0].snippet);
    }

    #[test]
    fn search_conversation_history_keyword_is_case_insensitive() {
        let s = new_server();
        insert_email(&s, "m1", "discord", "Rust Talk", "Body mentioning Rust", 1_700_000_000_000);
        let hits = s
            .search_conversation_history(Some("RUST"), None, None, None, None)
            .expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_conversation_history_filters_by_channel() {
        let s = new_server();
        insert_email(&s, "m1", "discord", "hi", "hello there from discord", 1_700_000_000_000);
        insert_email(&s, "m2", "gmail", "hi", "hello there from email", 1_700_000_100_000);
        let hits = s
            .search_conversation_history(Some("hello"), None, None, Some("discord"), None)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].channel, "discord");
    }

    #[test]
    fn search_conversation_history_filters_by_date_range() {
        let s = new_server();
        // Three messages straddling 2025-05-15.
        insert_email(&s, "m1", "gmail", "before", "old chat", 1_715_000_000_000); // May 6 2024 ish — well before
        insert_email(&s, "m2", "gmail", "middle", "mid chat", 1_747_440_000_000); // May 17 2025
        insert_email(&s, "m3", "gmail", "after", "new chat", 1_900_000_000_000);
        let since = 1_747_000_000_000;
        let until = 1_748_000_000_000;
        let hits = s
            .search_conversation_history(None, Some(since), Some(until), None, None)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "middle: mid chat");
    }

    #[test]
    fn search_conversation_history_returns_agent_drafts() {
        let s = new_server();
        insert_email(&s, "m1", "discord", "ping", "user pinged us", 1_700_000_000_000);
        insert_action(
            &s,
            "a1",
            "m1",
            "re: ping",
            "agent drafted reply mentioning meeting time",
            1_700_000_050_000,
        );
        let hits = s
            .search_conversation_history(Some("meeting"), None, None, None, None)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].role, "agent");
        assert_eq!(
            hits[0].channel, "discord",
            "agent draft inherits channel from joined emails row"
        );
    }

    #[test]
    fn search_conversation_history_orders_newest_first() {
        let s = new_server();
        insert_email(&s, "m1", "gmail", "first", "match foo body", 1_700_000_000_000);
        insert_email(&s, "m2", "gmail", "second", "match foo body", 1_800_000_000_000);
        let hits = s
            .search_conversation_history(Some("foo"), None, None, None, None)
            .expect("search");
        assert_eq!(hits.len(), 2);
        assert!(hits[0].timestamp_ms > hits[1].timestamp_ms);
    }

    #[test]
    fn search_conversation_history_respects_limit_clamp() {
        let s = new_server();
        for i in 0..30 {
            insert_email(
                &s,
                &format!("m{i}"),
                "gmail",
                "foo",
                "match foo body",
                1_700_000_000_000 + i,
            );
        }
        let hits = s
            .search_conversation_history(Some("foo"), None, None, None, Some(5))
            .expect("search");
        assert_eq!(hits.len(), 5);
        // Over-cap clamps to MAX_LIMIT, not the supplied value.
        let hits = s
            .search_conversation_history(Some("foo"), None, None, None, Some(9999))
            .expect("search");
        assert!(hits.len() <= MAX_LIMIT);
        assert_eq!(hits.len(), 30, "all rows fit under MAX_LIMIT here");
    }

    #[test]
    fn search_conversation_history_snippet_truncates_with_ellipsis() {
        let s = new_server();
        let long = "x".repeat(SNIPPET_MAX_CHARS + 100);
        insert_email(&s, "m1", "gmail", "", &long, 1_700_000_000_000);
        let hits = s
            .search_conversation_history(Some("x"), None, None, None, None)
            .expect("search");
        assert_eq!(hits.len(), 1);
        // Ellipsis is one extra char beyond SNIPPET_MAX_CHARS.
        let count = hits[0].snippet.chars().count();
        assert_eq!(count, SNIPPET_MAX_CHARS + 1);
        assert!(hits[0].snippet.ends_with('…'));
    }

    #[test]
    fn parse_date_to_ms_accepts_yyyymmdd() {
        // 2025-01-15T00:00:00Z = 1736899200000 ms
        let start = parse_date_to_ms("2025-01-15", false).unwrap();
        assert_eq!(start, 1_736_899_200_000);
        // end-of-day pushes to 23:59:59.999.
        let end = parse_date_to_ms("2025-01-15", true).unwrap();
        assert_eq!(end - start, 86_399_999);
    }

    #[test]
    fn parse_date_to_ms_accepts_rfc3339() {
        let ms = parse_date_to_ms("2025-01-15T12:00:00Z", false).unwrap();
        assert_eq!(ms, 1_736_942_400_000);
    }

    #[test]
    fn parse_date_to_ms_rejects_junk() {
        assert!(parse_date_to_ms("not a date", false).is_err());
        assert!(parse_date_to_ms("2025-13-40", false).is_err());
    }

    #[test]
    fn dispatch_search_conversation_history_round_trips() {
        let s = new_server();
        insert_email(
            &s,
            "m1",
            "discord",
            "kickoff",
            "discussed the launch plan",
            1_700_000_000_000,
        );
        let req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(7),
            method: "tools/call".into(),
            params: Some(json!({
                "name": "search_conversation_history",
                "arguments": { "keyword": "launch" }
            })),
        };
        let resp = s.dispatch(&req);
        let body = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("search text");
        assert!(body.contains("launch"), "expected hit in body: {body}");
        assert!(body.contains("\"channel\": \"discord\""), "channel: {body}");
    }

    #[test]
    fn dispatch_search_conversation_history_propagates_filter_error() {
        let s = new_server();
        let req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(8),
            method: "tools/call".into(),
            params: Some(json!({
                "name": "search_conversation_history",
                "arguments": {}
            })),
        };
        let resp = s.dispatch(&req);
        // Missing-filter is a tool-level (`-32000`) error, not a protocol
        // error — the call reached the handler, it just refused.
        assert_eq!(resp["error"]["code"], -32000);
        let msg = resp["error"]["message"].as_str().unwrap_or("");
        assert!(msg.contains("keyword"), "got: {msg}");
    }

    #[test]
    fn dispatch_unknown_tool_returns_jsonrpc_error() {
        let s = new_server();
        let req = McpRequest {
            jsonrpc: "2.0".into(),
            id: json!(100),
            method: "tools/call".into(),
            params: Some(json!({ "name": "memory_make_up_a_thing" })),
        };
        let resp = s.dispatch(&req);
        assert_eq!(resp["error"]["code"], -32602);
    }
}

#[cfg(test)]
mod conversation_thread_id_tests {
    use super::tests::*;
    use super::*;

    /// The bug this fixes: the agent recalled a LinkedIn DM's text through
    /// this tool, then reported it could not reply because it had no
    /// conversation urn — while the urn sat in the very row it had read.
    /// `thread_id` was in `emails` and simply never selected.
    #[test]
    fn search_returns_the_thread_id_a_reply_needs() {
        let s = new_server();
        insert_email_threaded(
            &s,
            "urn:li:msg:111",
            "linkedin",
            "[LinkedIn DM from Mansi Pathak]",
            "hi, following Code and Coffee",
            1_700_000_000_000,
            Some("urn:li:msg_conversation:999"),
        );
        let hits = s
            .search_conversation_history(Some("code and coffee"), None, None, None, Some(10))
            .expect("search");
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(
            hit.thread_id.as_deref(),
            Some("urn:li:msg_conversation:999"),
            "the conversation urn must come back — it is what `linkedin dm \
             --conversation-urn` requires"
        );
        assert_eq!(hit.message_id.as_deref(), Some("urn:li:msg:111"));
    }

    /// A row with no thread is `None`, not an empty string — a caller must
    /// never pass "" to `--conversation-urn` believing it has an id.
    #[test]
    fn absent_thread_id_is_none_not_empty_string() {
        let s = new_server();
        insert_email(&s, "m1", "discord", "subj", "unthreaded body", 1_700_000_000_000);
        let hits = s
            .search_conversation_history(Some("unthreaded"), None, None, None, Some(10))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].thread_id.is_none());
    }

    /// Blank-but-present ids are normalized to None for the same reason.
    #[test]
    fn blank_thread_id_is_normalized_to_none() {
        let s = new_server();
        insert_email_threaded(
            &s,
            "m2",
            "linkedin",
            "subj",
            "blankthread body",
            1_700_000_000_000,
            Some("   "),
        );
        let hits = s
            .search_conversation_history(Some("blankthread"), None, None, None, Some(10))
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].thread_id.is_none(), "whitespace is not a usable id");
    }
}
