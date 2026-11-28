//! Jarvis Dashboard chat store (spec web-dashboard-v2 §5.1). The daemon is
//! the only writer; the dashboard reads these tables on its own read-only
//! connection. All timestamps are epoch milliseconds.
use crate::store::now_millis;
use crate::{
    Store, StoreError, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef,
    SurfaceTurnResolution, SurfaceTurnStatus,
};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use sha2::{Digest, Sha256};

const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_OPEN_TURNS: i64 = 3;
const MAX_TITLE_CHARS: usize = 120;
const AUTO_TITLE_CHARS: usize = 60;
const MAX_TURN_ID_BYTES: usize = 128;
const HISTORY_MESSAGES: i64 = 50;
const HISTORY_BYTES: usize = 16 * 1024;
const DEFAULT_TITLE: &str = "New thread";

pub(crate) fn migrate(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS web_threads (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            title_set INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            changed_at INTEGER NOT NULL,
            pinned INTEGER NOT NULL DEFAULT 0,
            archived INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS web_chat_messages (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_id TEXT NOT NULL REFERENCES web_threads(id),
            turn_id TEXT NOT NULL,
            role TEXT NOT NULL CHECK(role IN ('owner','jarvis')),
            text TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(turn_id, role));
        CREATE TABLE IF NOT EXISTS web_turns (
            turn_id TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL REFERENCES web_threads(id),
            text_sha256 TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('queued','running','done','failed','cancelled')),
            error_code TEXT NULL,
            error TEXT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            finished_at INTEGER NULL);
        CREATE INDEX IF NOT EXISTS web_turns_updated ON web_turns(updated_at);
        CREATE INDEX IF NOT EXISTS web_turns_thread ON web_turns(thread_id, status);
        CREATE INDEX IF NOT EXISTS web_chat_messages_thread ON web_chat_messages(thread_id, seq);",
    )?;
    Ok(())
}

/// Store refusals, one per socket error code (§5.2.1).
#[derive(Debug, thiserror::Error)]
pub enum WebChatError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("thread not found")]
    ThreadNotFound,
    #[error("thread is archived")]
    ThreadArchived,
    #[error("text is larger than 32 KiB")]
    TextTooLarge,
    #[error("thread already has 3 unfinished turns")]
    ThreadBusy,
    #[error("turn id was already used for a different message")]
    TurnConflict,
    #[error("turn not found")]
    TurnNotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for WebChatError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Store(StoreError::Sqlite(err))
    }
}

impl WebChatError {
    /// The socket error code for this refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::ThreadNotFound => "thread_not_found",
            Self::ThreadArchived => "thread_archived",
            Self::TextTooLarge => "text_too_large",
            Self::ThreadBusy => "thread_busy",
            Self::TurnConflict => "turn_conflict",
            Self::TurnNotFound => "turn_not_found",
            Self::Store(_) => "internal",
        }
    }
}

pub type WebChatResult<T> = Result<T, WebChatError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebThread {
    pub id: String,
    pub title: String,
    pub title_set: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub changed_at: i64,
    pub pinned: bool,
    pub archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTurnStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl WebTurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        Ok(match value {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            other => return Err(bad_column(format!("unknown web turn status {other}"))),
        })
    }
}

/// `web_turns.error_code` (§5.2.1 "Turn error codes").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTurnErrorCode {
    Interrupted,
    UncertainSession,
    ProviderError,
    Internal,
}

impl WebTurnErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::UncertainSession => "uncertain_session",
            Self::ProviderError => "provider_error",
            Self::Internal => "internal",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        Ok(match value {
            "interrupted" => Self::Interrupted,
            "uncertain_session" => Self::UncertainSession,
            "provider_error" => Self::ProviderError,
            "internal" => Self::Internal,
            other => return Err(bad_column(format!("unknown web turn error {other}"))),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTurn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: WebTurnStatus,
    pub error_code: Option<WebTurnErrorCode>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

/// How a running turn ended (§5.2.1 Runner: exactly one terminal write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebTurnEnd<'a> {
    /// The reply, stored as the turn's `jarvis` message.
    Done(&'a str),
    Failed(WebTurnErrorCode, &'a str),
    Cancelled,
}

const OPEN: &[WebTurnStatus] = &[WebTurnStatus::Queued, WebTurnStatus::Running];

/// `turn.submit` result. `fresh` is false for an idempotent replay, which
/// must not spawn a runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSubmitted {
    pub status: WebTurnStatus,
    pub fresh: bool,
}

/// The surface conversation a web thread runs in (spec D1).
pub fn web_conversation(thread_id: &str) -> SurfaceConversationRef {
    let platform = SurfacePlatform::new("web").expect("static platform");
    let account = SurfaceAccountRef::new(platform, "owner").expect("static account");
    SurfaceConversationRef::new(account, thread_id, None).expect("thread ids are non-empty")
}

/// The surface turn claim for a web turn: `web:<turn_id>` (§5.2.1 Runner).
pub fn web_turn_ref(thread_id: &str, turn_id: &str) -> SurfaceTurnRef {
    SurfaceTurnRef::new(web_conversation(thread_id), format!("web:{turn_id}"))
        .expect("turn ref is non-empty")
}

fn bad_column(message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, message.into())
}

fn uuid_v7(now: i64) -> String {
    let mut bytes = *uuid::Uuid::new_v4().as_bytes();
    bytes[..6].copy_from_slice(&(now as u64).to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn clean_title(title: &str) -> WebChatResult<String> {
    let title = title.trim();
    let chars = title.chars().count();
    if chars == 0 || chars > MAX_TITLE_CHARS {
        return Err(WebChatError::BadRequest(
            "title must be 1..120 characters".into(),
        ));
    }
    Ok(title.to_string())
}

fn auto_title(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(AUTO_TITLE_CHARS)
        .collect()
}

const THREAD_COLS: &str =
    "id, title, title_set, created_at, updated_at, changed_at, pinned, archived";
const TURN_COLS: &str =
    "turn_id, thread_id, status, error_code, error, created_at, updated_at, finished_at";

fn thread_row(row: &Row) -> rusqlite::Result<WebThread> {
    Ok(WebThread {
        id: row.get(0)?,
        title: row.get(1)?,
        title_set: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        changed_at: row.get(5)?,
        pinned: row.get(6)?,
        archived: row.get(7)?,
    })
}

fn turn_row(row: &Row) -> rusqlite::Result<WebTurn> {
    Ok(WebTurn {
        turn_id: row.get(0)?,
        thread_id: row.get(1)?,
        status: WebTurnStatus::parse(&row.get::<_, String>(2)?)?,
        error_code: row
            .get::<_, Option<String>>(3)?
            .as_deref()
            .map(WebTurnErrorCode::parse)
            .transpose()?,
        error: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        finished_at: row.get(7)?,
    })
}

fn load_thread(conn: &Connection, id: &str) -> WebChatResult<WebThread> {
    conn.query_row(
        &format!("SELECT {THREAD_COLS} FROM web_threads WHERE id = ?1"),
        [id],
        thread_row,
    )
    .optional()?
    .ok_or(WebChatError::ThreadNotFound)
}

fn load_turn(conn: &Connection, turn_id: &str) -> WebChatResult<WebTurn> {
    conn.query_row(
        &format!("SELECT {TURN_COLS} FROM web_turns WHERE turn_id = ?1"),
        [turn_id],
        turn_row,
    )
    .optional()?
    .ok_or(WebChatError::TurnNotFound)
}

/// Move a turn from one of `from` to `to` and bump its thread's activity, in
/// the caller's transaction. Returns false when the turn was not in `from`.
fn transition(
    conn: &Connection,
    turn_id: &str,
    from: &[WebTurnStatus],
    to: WebTurnStatus,
    error: Option<(WebTurnErrorCode, &str)>,
    now: i64,
) -> WebChatResult<bool> {
    let turn = load_turn(conn, turn_id)?;
    if !from.contains(&turn.status) {
        return Ok(false);
    }
    let terminal = !matches!(to, WebTurnStatus::Queued | WebTurnStatus::Running);
    conn.execute(
        "UPDATE web_turns SET status = ?2, error_code = ?3, error = ?4, updated_at = ?5,
         finished_at = CASE WHEN ?6 THEN ?5 ELSE finished_at END
         WHERE turn_id = ?1 AND status = ?7",
        params![
            turn_id,
            to.as_str(),
            error.map(|(code, _)| code.as_str()),
            error.map(|(_, text)| text),
            now,
            terminal,
            turn.status.as_str()
        ],
    )?;
    conn.execute(
        "UPDATE web_threads SET updated_at = ?2, changed_at = ?2 WHERE id = ?1",
        params![turn.thread_id, now],
    )?;
    Ok(true)
}

impl Store {
    fn web_tx<T>(&self, f: impl FnOnce(&Connection) -> WebChatResult<T>) -> WebChatResult<T> {
        let mut guard = self.conn.lock().expect("store mutex poisoned");
        let tx = guard.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn web_create_thread(&self, title: Option<&str>) -> WebChatResult<WebThread> {
        let (title, title_set) = match title {
            Some(title) => (clean_title(title)?, true),
            None => (DEFAULT_TITLE.to_string(), false),
        };
        let now = now_millis();
        let id = uuid_v7(now);
        self.web_tx(|conn| {
            conn.execute(
                "INSERT INTO web_threads (id, title, title_set, created_at, updated_at, changed_at)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?4)",
                params![id, title, title_set, now],
            )?;
            load_thread(conn, &id)
        })
    }

    pub fn web_rename_thread(&self, id: &str, title: &str) -> WebChatResult<WebThread> {
        let title = clean_title(title)?;
        let now = now_millis();
        self.web_tx(|conn| {
            load_thread(conn, id)?;
            conn.execute(
                "UPDATE web_threads SET title = ?2, title_set = 1, updated_at = ?3, changed_at = ?3
                 WHERE id = ?1",
                params![id, title, now],
            )?;
            load_thread(conn, id)
        })
    }

    /// Pin/archive. Idempotent: `changed_at` moves only when a flag changes.
    pub fn web_set_thread(
        &self,
        id: &str,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> WebChatResult<WebThread> {
        let now = now_millis();
        self.web_tx(|conn| {
            let current = load_thread(conn, id)?;
            let pinned = pinned.unwrap_or(current.pinned);
            let archived = archived.unwrap_or(current.archived);
            if pinned != current.pinned || archived != current.archived {
                conn.execute(
                    "UPDATE web_threads SET pinned = ?2, archived = ?3, changed_at = ?4 WHERE id = ?1",
                    params![id, pinned, archived, now],
                )?;
            }
            load_thread(conn, id)
        })
    }

    pub fn web_thread(&self, id: &str) -> WebChatResult<WebThread> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        load_thread(&guard, id)
    }

    /// Every thread, pinned first, then most recent activity.
    pub fn web_threads(&self) -> WebChatResult<Vec<WebThread>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(&format!(
            "SELECT {THREAD_COLS} FROM web_threads ORDER BY pinned DESC, updated_at DESC, id DESC"
        ))?;
        let rows = stmt
            .query_map([], thread_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn web_turn(&self, turn_id: &str) -> WebChatResult<WebTurn> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        load_turn(&guard, turn_id)
    }

    /// `turn.submit` steps 1–4 (§5.2.1) in one transaction.
    pub fn web_submit_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        text: &str,
    ) -> WebChatResult<WebSubmitted> {
        if turn_id.is_empty() || turn_id.len() > MAX_TURN_ID_BYTES {
            return Err(WebChatError::BadRequest(
                "turn_id must be 1..128 bytes".into(),
            ));
        }
        let sha = sha256_hex(text);
        let now = now_millis();
        self.web_tx(|conn| {
            let existing: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT thread_id, text_sha256, status FROM web_turns WHERE turn_id = ?1",
                    [turn_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((stored_thread, stored_sha, status)) = existing {
                if stored_thread != thread_id || stored_sha != sha {
                    return Err(WebChatError::TurnConflict);
                }
                let status = WebTurnStatus::parse(&status)?;
                return Ok(WebSubmitted { status, fresh: false });
            }
            let thread = load_thread(conn, thread_id)?;
            if thread.archived {
                return Err(WebChatError::ThreadArchived);
            }
            if text.len() > MAX_TEXT_BYTES {
                return Err(WebChatError::TextTooLarge);
            }
            if text.trim().is_empty() {
                return Err(WebChatError::BadRequest("text is empty".into()));
            }
            let open: i64 = conn.query_row(
                "SELECT COUNT(*) FROM web_turns WHERE thread_id = ?1
                 AND status IN ('queued','running')",
                [thread_id],
                |r| r.get(0),
            )?;
            if open >= MAX_OPEN_TURNS {
                return Err(WebChatError::ThreadBusy);
            }
            conn.execute(
                "INSERT INTO web_chat_messages (thread_id, turn_id, role, text, created_at)
                 VALUES (?1, ?2, 'owner', ?3, ?4)",
                params![thread_id, turn_id, text, now],
            )?;
            conn.execute(
                "INSERT INTO web_turns (turn_id, thread_id, text_sha256, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'queued', ?4, ?4)",
                params![turn_id, thread_id, sha, now],
            )?;
            if !thread.title_set {
                conn.execute(
                    "UPDATE web_threads SET title = ?2, title_set = 1 WHERE id = ?1",
                    params![thread_id, auto_title(text)],
                )?;
            }
            conn.execute(
                "UPDATE web_threads SET updated_at = ?2, changed_at = ?2 WHERE id = ?1",
                params![thread_id, now],
            )?;
            Ok(WebSubmitted { status: WebTurnStatus::Queued, fresh: true })
        })
    }

    /// Runner CAS `queued → running`. False means it was cancelled first.
    pub fn web_start_turn(&self, turn_id: &str) -> WebChatResult<bool> {
        let now = now_millis();
        self.web_tx(|conn| {
            transition(
                conn,
                turn_id,
                &[WebTurnStatus::Queued],
                WebTurnStatus::Running,
                None,
                now,
            )
        })
    }

    /// `turn.cancel`'s store half: CAS `queued → cancelled`, then return the
    /// turn's current status (a running turn is cancelled by its runner).
    pub fn web_cancel_queued_turn(&self, turn_id: &str) -> WebChatResult<WebTurnStatus> {
        let now = now_millis();
        self.web_tx(|conn| {
            transition(
                conn,
                turn_id,
                &[WebTurnStatus::Queued],
                WebTurnStatus::Cancelled,
                None,
                now,
            )?;
            Ok(load_turn(conn, turn_id)?.status)
        })
    }

    /// The runner's one terminal write. `Done` (from `running` only) stores
    /// the `jarvis` message in the same transaction. False when the turn had
    /// already left the states the end applies to.
    pub fn web_end_turn(&self, turn_id: &str, end: WebTurnEnd<'_>) -> WebChatResult<bool> {
        let now = now_millis();
        let (from, to, error): (&[WebTurnStatus], _, _) = match end {
            WebTurnEnd::Done(_) => (&[WebTurnStatus::Running], WebTurnStatus::Done, None),
            WebTurnEnd::Failed(code, text) => (OPEN, WebTurnStatus::Failed, Some((code, text))),
            WebTurnEnd::Cancelled => (OPEN, WebTurnStatus::Cancelled, None),
        };
        self.web_tx(|conn| {
            if !transition(conn, turn_id, from, to, error, now)? {
                return Ok(false);
            }
            if let WebTurnEnd::Done(reply) = end {
                conn.execute(
                    "INSERT INTO web_chat_messages (thread_id, turn_id, role, text, created_at)
                     SELECT thread_id, turn_id, 'jarvis', ?2, ?3 FROM web_turns WHERE turn_id = ?1",
                    params![turn_id, reply, now],
                )?;
            }
            Ok(true)
        })
    }

    /// Fallback history for `current_turn_id` (§5.1): messages before its
    /// owner row, oldest first, labelled by role, at most 50 messages and
    /// 16 KiB; one oversize message is cut to 16 KiB ending in `…`.
    pub fn web_fallback_history(
        &self,
        thread_id: &str,
        current_turn_id: &str,
    ) -> WebChatResult<String> {
        let rows: Vec<(String, String)> = self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT role, text FROM web_chat_messages WHERE thread_id = ?1
                 AND seq < COALESCE((SELECT seq FROM web_chat_messages
                     WHERE turn_id = ?2 AND role = 'owner'), 9223372036854775807)
                 ORDER BY seq DESC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![thread_id, current_turn_id, HISTORY_MESSAGES], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        let mut selected = Vec::new();
        let mut bytes = 0;
        for (role, text) in rows {
            let label = if role == "owner" { "Owner" } else { "Jarvis" };
            let mut entry = format!("{label}: {text}");
            if entry.len() > HISTORY_BYTES {
                let mut end = HISTORY_BYTES - '…'.len_utf8();
                while !entry.is_char_boundary(end) {
                    end -= 1;
                }
                entry.truncate(end);
                entry.push('…');
            }
            let sep = usize::from(!selected.is_empty());
            if bytes + sep + entry.len() > HISTORY_BYTES {
                break;
            }
            bytes += sep + entry.len();
            selected.push(entry);
        }
        selected.reverse();
        Ok(selected.join("\n"))
    }

    /// Daemon start, before the socket opens: every unfinished turn fails as
    /// `interrupted`; a surface claim still `Pending` is resolved as
    /// interrupted, other claims (or none) are left alone. Returns the count.
    pub fn web_restart_sweep(&self) -> WebChatResult<usize> {
        let now = now_millis();
        let swept: Vec<(String, String)> = self.web_tx(|conn| {
            let mut stmt = conn.prepare(
                "SELECT turn_id, thread_id FROM web_turns WHERE status IN ('queued','running')",
            )?;
            let open: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (turn_id, _) in &open {
                transition(
                    conn,
                    turn_id,
                    OPEN,
                    WebTurnStatus::Failed,
                    Some((
                        WebTurnErrorCode::Interrupted,
                        "Jarvis restarted during this turn",
                    )),
                    now,
                )?;
            }
            Ok(open)
        })?;
        for (turn_id, thread_id) in &swept {
            let turn = web_turn_ref(thread_id, turn_id);
            let pending = matches!(
                self.surface_turn_state(&turn)?,
                Some(state) if state.status == SurfaceTurnStatus::Pending && state.resolution.is_none()
            );
            if pending {
                self.resolve_surface_turn(&turn, SurfaceTurnResolution::Interrupted)?;
            }
        }
        Ok(swept.len())
    }
}
