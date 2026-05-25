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

/// Pagination cap for any single call. Matches Claude Code's typical
/// per-tool reply budget so a runaway `memory_search` doesn't blow out
/// the context window.
pub const MAX_LIMIT: usize = 100;

/// Default limit when the caller doesn't supply one.
pub const DEFAULT_LIMIT: usize = 10;

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
                stmt.query_map(params![s.trim(), limit as i64], row_to_memory_unscored)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            _ => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, created_at_ms, surface, subject, body, tags \
                     FROM memory \
                     ORDER BY created_at_ms DESC \
                     LIMIT ?1",
                )?;
                stmt.query_map(params![limit as i64], row_to_memory_unscored)?
                    .collect::<Result<Vec<_>, _>>()?
            }
        };
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
    match limit {
        None => DEFAULT_LIMIT,
        Some(0) => DEFAULT_LIMIT,
        Some(n) if n > MAX_LIMIT => MAX_LIMIT,
        Some(n) => n,
    }
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
    struct Fixture {
        server: Server,
        _tmp: TempDir,
    }

    impl std::ops::Deref for Fixture {
        type Target = Server;
        fn deref(&self) -> &Server {
            &self.server
        }
    }

    fn new_server() -> Fixture {
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
    fn dispatch_tools_list_enumerates_three_tools() {
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
