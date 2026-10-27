//! Durable owner-phone state. Provider I/O happens only after a transaction
//! grants a single dial permit; a process death never recreates that permit.
//! Destinations are deliberately absent from serializable call receipts.
use crate::{Store, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use thiserror::Error;

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(include_str!("phone.sql"))?;
    // #1555 speculation fields, added in place to existing metrics tables.
    for (column, definition) in [
        ("speculated", "INTEGER NOT NULL DEFAULT 0 CHECK(speculated IN (0,1))"),
        (
            "speculation_committed",
            "INTEGER NOT NULL DEFAULT 0 CHECK(speculation_committed IN (0,1))",
        ),
        (
            "speculation_saved_ms",
            "INTEGER CHECK(speculation_saved_ms IS NULL OR speculation_saved_ms >= 0)",
        ),
    ] {
        let present: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('phone_turn_metrics') WHERE name = ?1",
            [column],
            |row| row.get(0),
        )?;
        if !present {
            conn.execute(
                &format!("ALTER TABLE phone_turn_metrics ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    allow_silence_end_reason(conn)?;
    Ok(())
}
/// #1556: a `phone_call_end_reasons` table created by #1557 rejects
/// 'silence' in its CHECK. SQLite cannot alter a CHECK, so the table is
/// rebuilt once with its rows kept. A no-op on any later open.
fn allow_silence_end_reason(conn: &Connection) -> StoreResult<()> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='phone_call_end_reasons'",
        [],
        |r| r.get(0),
    )?;
    if sql.contains("'silence'") {
        return Ok(());
    }
    // Rolled back on any error, so a failed rebuild leaves the old table.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "ALTER TABLE phone_call_end_reasons RENAME TO phone_call_end_reasons_1557;
         CREATE TABLE phone_call_end_reasons (
             call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
             reason TEXT NOT NULL CHECK(reason IN ('owner_goodbye','task_complete','owner_request','relay_ended_unknown','silence')),
             recorded_ms INTEGER NOT NULL
         );
         INSERT INTO phone_call_end_reasons(call_id,reason,recorded_ms)
             SELECT call_id,reason,recorded_ms FROM phone_call_end_reasons_1557;
         DROP TABLE phone_call_end_reasons_1557;",
    )?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum PhoneError {
    #[error("phone destination is not configured")]
    NotConfigured,
    #[error("phone request conflicts with existing state")]
    Conflict,
    #[error("an owner call is already pending or active")]
    AlreadyActive,
    #[error("phone rate limit reached")]
    RateLimited,
    #[error("daily phone allowance exhausted")]
    BudgetExhausted,
    #[error("phone call not found")]
    NotFound,
    #[error("invalid phone input")]
    InvalidInput,
    #[error("phone acceptance deadline expired")]
    AcceptanceExpired,
    #[error("phone storage failed")]
    Storage(#[from] StoreError),
    #[error("phone database operation failed")]
    Database(#[from] rusqlite::Error),
}
pub type PhoneResult<T> = Result<T, PhoneError>;

/// Explicit access only. Debug/error/receipt paths must never reveal numbers.
#[derive(Clone)]
pub struct PhoneDestination(String);
impl PhoneDestination {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for PhoneDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PhoneDestination([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Requesting,
    Uncertain,
    Queued,
    Ringing,
    AwaitingAcceptance,
    Connected,
    Completed,
    Busy,
    NoAnswer,
    Canceled,
    Failed,
}
impl CallState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requesting => "requesting",
            Self::Uncertain => "uncertain",
            Self::Queued => "queued",
            Self::Ringing => "ringing",
            Self::AwaitingAcceptance => "awaiting_acceptance",
            Self::Connected => "connected",
            Self::Completed => "completed",
            Self::Busy => "busy",
            Self::NoAnswer => "no_answer",
            Self::Canceled => "canceled",
            Self::Failed => "failed",
        }
    }
    pub fn terminal(self) -> bool {
        self.rank() == 6
    }
    fn rank(self) -> u8 {
        match self {
            Self::Requesting | Self::Uncertain => 0,
            Self::Queued => 1,
            Self::Ringing => 2,
            Self::AwaitingAcceptance => 3,
            Self::Connected => 4,
            _ => 6,
        }
    }
    fn parse(s: &str) -> rusqlite::Result<Self> {
        Ok(match s {
            "requesting" => Self::Requesting,
            "uncertain" => Self::Uncertain,
            "queued" => Self::Queued,
            "ringing" => Self::Ringing,
            "awaiting_acceptance" => Self::AwaitingAcceptance,
            "connected" => Self::Connected,
            "completed" => Self::Completed,
            "busy" => Self::Busy,
            "no_answer" => Self::NoAnswer,
            "canceled" => Self::Canceled,
            "failed" => Self::Failed,
            _ => return Err(rusqlite::Error::InvalidQuery),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PhoneCall {
    pub id: String,
    pub owner: String,
    pub request_id: String,
    pub conversation: String,
    pub provider_sid: Option<String>,
    pub state: CallState,
    pub created_ms: i64,
    pub answered_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub max_duration_seconds: i64,
    pub stop_requested: bool,
    pub verification: bool,
}
/// Final duration and event time from an authenticated provider callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderUsage {
    pub duration_seconds: i64,
    pub ended_ms: i64,
}

#[derive(Debug)]
pub struct CallReservation {
    pub call: PhoneCall,
    pub should_dial: bool,
}
pub struct ReserveCall<'a> {
    pub owner: &'a str,
    pub request_id: &'a str,
    pub conversation: &'a str,
    pub now_ms: i64,
}
#[derive(Debug, Serialize)]
pub struct DestinationStatus {
    pub masked: Option<String>,
    pub pending: bool,
}

pub struct BeginVerification<'a> {
    pub request: ReserveCall<'a>,
    pub number: &'a str,
    pub code_hash: &'a [u8; 32],
}

fn identifier(s: &str) -> PhoneResult<()> {
    if s.is_empty() || s.len() > 512 || s.chars().any(char::is_control) {
        return Err(PhoneError::InvalidInput);
    }
    Ok(())
}
pub fn validate_e164(s: &str) -> PhoneResult<()> {
    if !(9..=16).contains(&s.len())
        || !s.starts_with('+')
        || !matches!(s.as_bytes()[1], b'1'..=b'9')
        || !s.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(PhoneError::InvalidInput);
    }
    Ok(())
}
fn transaction<T>(
    store: &Store,
    f: impl FnOnce(&Transaction<'_>) -> PhoneResult<T>,
) -> PhoneResult<T> {
    store.with_conn(|conn| {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        match f(&tx) {
            Ok(v) => {
                tx.commit()?;
                Ok(Ok(v))
            }
            Err(e) => Ok(Err(e)),
        }
    })?
}
const CALL_COLUMNS: &str = "id,owner,request_id,conversation,provider_sid,state,created_ms,answered_ms,ended_ms,max_duration_seconds,stop_requested,verification";
fn read_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<PhoneCall> {
    Ok(PhoneCall {
        id: row.get(0)?,
        owner: row.get(1)?,
        request_id: row.get(2)?,
        conversation: row.get(3)?,
        provider_sid: row.get(4)?,
        state: CallState::parse(&row.get::<_, String>(5)?)?,
        created_ms: row.get(6)?,
        answered_ms: row.get(7)?,
        ended_ms: row.get(8)?,
        max_duration_seconds: row.get(9)?,
        stop_requested: row.get(10)?,
        verification: row.get(11)?,
    })
}
fn call_by_id(conn: &Connection, id: &str) -> PhoneResult<PhoneCall> {
    conn.query_row(
        &format!("SELECT {CALL_COLUMNS} FROM phone_calls WHERE id=?1"),
        [id],
        read_call,
    )
    .optional()?
    .ok_or(PhoneError::NotFound)
}

fn reserve(
    tx: &Transaction<'_>,
    request: ReserveCall<'_>,
    destination: Option<&str>,
    verification: bool,
) -> PhoneResult<CallReservation> {
    let existing = tx
        .query_row(
            &format!("SELECT {CALL_COLUMNS} FROM phone_calls WHERE owner=?1 AND request_id=?2"),
            params![request.owner, request.request_id],
            read_call,
        )
        .optional()?;
    if let Some(call) = existing {
        if call.verification != verification || call.conversation != request.conversation {
            return Err(PhoneError::Conflict);
        }
        if let Some(number) = destination {
            let old: String = tx.query_row(
                "SELECT destination FROM phone_calls WHERE id=?1",
                [&call.id],
                |r| r.get(0),
            )?;
            if old != number {
                return Err(PhoneError::Conflict);
            }
        }
        return Ok(CallReservation {
            call,
            should_dial: false,
        });
    }
    let number: Option<String> = tx
        .query_row(
            "SELECT number FROM phone_destinations WHERE owner=?1",
            [request.owner],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let number = destination
        .map(str::to_owned)
        .or(number)
        .ok_or(PhoneError::NotConfigured)?;
    let active: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM phone_calls WHERE owner=?1 AND ended_ms IS NULL)",
        [request.owner],
        |r| r.get(0),
    )?;
    if active {
        return Err(PhoneError::AlreadyActive);
    }
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM phone_calls WHERE owner=?1 AND created_ms>?2",
        params![request.owner, request.now_ms - 600_000],
        |r| r.get(0),
    )?;
    if count >= 3 {
        return Err(PhoneError::RateLimited);
    }
    let used_ms = used_budget_ms(tx, request.owner, request.now_ms)?;
    let seconds = ((3_600_000 - used_ms) / 1000).min(if verification { 30 } else { 900 });
    if seconds <= 0 {
        return Err(PhoneError::BudgetExhausted);
    }
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute("INSERT INTO phone_calls(id,owner,request_id,conversation,destination,state,created_ms,max_duration_seconds,verification) VALUES(?1,?2,?3,?4,?5,'requesting',?6,?7,?8)",
                params![id,request.owner,request.request_id,request.conversation,number,request.now_ms,seconds,verification])?;
    Ok(CallReservation {
        call: call_by_id(tx, &id)?,
        should_dial: true,
    })
}

// Verified duration is allocated by provider event time, not callback arrival.
// Until final usage arrives, reserve the full limit for potentially answered
// terminal calls; a missed answer callback must never create free call minutes.
fn used_budget_ms(conn: &Connection, owner: &str, now_ms: i64) -> rusqlite::Result<i64> {
    let day_start = now_ms / 86_400_000 * 86_400_000;
    conn.query_row(
                "SELECT COALESCE(SUM(CASE
                    WHEN u.call_id IS NOT NULL THEN MAX(0,MIN(u.ended_ms,?3+86400000)-MAX(u.ended_ms-u.duration_seconds*1000,?3))
                    WHEN c.ended_ms IS NOT NULL AND (c.answered_ms IS NOT NULL OR c.state IN ('completed','canceled'))
                        THEN CASE WHEN c.ended_ms>?3 AND c.created_ms<=?2 THEN MAX(c.max_duration_seconds*1000,MAX(0,MIN(c.ended_ms,?2)-MAX(COALESCE(c.answered_ms,c.ended_ms),?3))) ELSE 0 END
                    WHEN c.answered_ms IS NOT NULL THEN MAX(0,?2-MAX(c.answered_ms,?3))
                    ELSE 0 END),0)
                 FROM phone_calls c LEFT JOIN phone_call_usage u ON u.call_id=c.id WHERE c.owner=?1",
                params![owner,now_ms,day_start], |r|r.get(0))
}

impl Store {
    pub fn phone_prompt_for_number(&self, owner: &str, conversation: &str, now_ms: i64) -> PhoneResult<()> {
        identifier(owner)?;
        self.with_conn(|c| c.execute("INSERT INTO phone_setup_prompts VALUES(?1,?2,?3) ON CONFLICT(owner) DO UPDATE SET conversation=?2,created_ms=?3", params![owner,conversation,now_ms]))?;
        Ok(())
    }

    /// Authenticated owner-supplied destination, distinct from code verification.
    /// Replay receipts prevent an older delivered message replacing a newer number.
    pub fn phone_save_owner_number(&self, owner: &str, conversation: &str, event_id: &str, number: &str, call_now: bool, now_ms: i64) -> PhoneResult<Option<CallReservation>> {
        identifier(owner)?;
        identifier(event_id)?;
        validate_e164(number)?;
        transaction(self, |tx| {
            if let Some(call) = tx.query_row("SELECT should_call FROM phone_number_receipts WHERE owner=?1 AND event_id=?2", params![owner,event_id], |r|r.get::<_,bool>(0)).optional()? {
                if !call { return Ok(None); }
                // Retention may have removed the old call. Never recreate it.
                let old = tx.query_row(&format!("SELECT {CALL_COLUMNS} FROM phone_calls WHERE owner=?1 AND request_id=?2"), params![owner,event_id], read_call).optional()?;
                if old.as_ref().is_some_and(|call| call.conversation != conversation) {
                    return Err(PhoneError::Conflict);
                }
                return Ok(old.map(|call| CallReservation { call, should_dial: false }));
            }
            let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_calls WHERE owner=?1 AND state IN ('requesting','uncertain','queued','ringing','awaiting_acceptance','connected'))", [owner], |r|r.get(0))?;
            if active { return Err(PhoneError::AlreadyActive); }
            let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_setup_prompts WHERE owner=?1 AND conversation=?2 AND created_ms BETWEEN ?3 AND ?4)",params![owner,conversation,now_ms.saturating_sub(900_000),now_ms],|r|r.get(0))?;
            let call = call_now || pending;
            tx.execute("INSERT INTO phone_destinations VALUES(?1,?2,?3,'owner_supplied') ON CONFLICT(owner) DO UPDATE SET number=?2,updated_ms=?3,provenance='owner_supplied'", params![owner,number,now_ms])?;
            tx.execute("DELETE FROM phone_verifications WHERE owner=?1",[owner])?;
            tx.execute("DELETE FROM phone_setup_prompts WHERE owner=?1",[owner])?;
            tx.execute("INSERT INTO phone_number_receipts VALUES(?1,?2,?3)",params![owner,event_id,call])?;
            if call { reserve(tx, ReserveCall { owner, request_id: event_id, conversation, now_ms }, Some(number), false).map(Some) } else { Ok(None) }
        })
    }

    pub fn phone_budget_remaining_ms(&self, owner: &str, now_ms: i64) -> PhoneResult<i64> {
        identifier(owner)?;
        if now_ms < 0 {
            return Err(PhoneError::InvalidInput);
        }
        let used = self.with_conn(|conn| used_budget_ms(conn, owner, now_ms))?;
        Ok(3_600_000i64.saturating_sub(used).max(0))
    }

    /// Explicit one-time operator migration only. A tombstone left by removal
    /// prevents a restart from silently restoring an environment destination.
    pub fn phone_import_destination(
        &self,
        owner: &str,
        number: &str,
        now_ms: i64,
    ) -> PhoneResult<bool> {
        identifier(owner)?;
        validate_e164(number)?;
        Ok(self.with_conn(|c| c.execute(
            "INSERT OR IGNORE INTO phone_destinations(owner,number,updated_ms,provenance) VALUES(?1,?2,?3,'operator_import')",
            params![owner,number,now_ms]))? == 1)
    }
    pub fn phone_destination_status(&self, owner: &str) -> PhoneResult<DestinationStatus> {
        let number: Option<String> = self
            .with_conn(|c| {
                c.query_row(
                    "SELECT number FROM phone_destinations WHERE owner=?1",
                    [owner],
                    |r| r.get(0),
                )
                .optional()
            })?
            .flatten();
        let pending = self.with_conn(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_verifications WHERE owner=?1)",
                [owner],
                |r| r.get(0),
            )
        })?;
        Ok(DestinationStatus {
            masked: number.map(|n| format!("••••{}", &n[n.len() - 4..])),
            pending,
        })
    }
    pub fn phone_reserve_call(&self, request: ReserveCall<'_>) -> PhoneResult<CallReservation> {
        identifier(request.owner)?;
        identifier(request.request_id)?;
        identifier(request.conversation)?;
        if request.now_ms < 0 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| reserve(tx, request, None, false))
    }

    pub fn phone_call(&self, owner: &str, id: &str) -> PhoneResult<PhoneCall> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.owner != owner {
                return Err(PhoneError::NotFound);
            }
            Ok(call)
        })
    }
    /// Read-only operational evidence. Attempts count reservations, including
    /// any request still in flight; they do not assert provider completion.
    pub fn phone_maintenance_status(&self, owner: &str, id: &str) -> PhoneResult<PhoneMaintenanceStatus> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.owner != owner { return Err(PhoneError::NotFound); }
            let (cleanup_attempts, usage_attempts, final_usage_recorded) = tx.query_row(
                "SELECT COALESCE((SELECT attempts FROM phone_cleanup WHERE call_id=?1),0),
                 COALESCE((SELECT attempts FROM phone_usage_reconciliation WHERE call_id=?1),0),
                 EXISTS(SELECT 1 FROM phone_call_usage WHERE call_id=?1)",
                [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            Ok(PhoneMaintenanceStatus { provider_sid_known: call.provider_sid.is_some(),
                cleanup_attempts, usage_attempts, final_usage_recorded })
        })
    }
    /// For the provider adapter only; never serialize this into a tool receipt.
    pub fn phone_dial_destination(&self, id: &str) -> PhoneResult<PhoneDestination> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT destination FROM phone_calls WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
        })?
        .map(PhoneDestination)
        .ok_or(PhoneError::NotFound)
    }
    pub fn phone_mark_uncertain(&self, id: &str) -> PhoneResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE phone_calls SET state='uncertain' WHERE id=?1 AND state='requesting'",
                [id],
            )
        })?;
        Ok(())
    }
    pub fn phone_bind_provider(&self, id: &str, sid: &str) -> PhoneResult<()> {
        identifier(sid)?;
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.provider_sid.as_deref().is_some_and(|old| old != sid) {
                return Err(PhoneError::Conflict);
            }
            tx.execute(
                "UPDATE phone_calls SET provider_sid=?2 WHERE id=?1",
                params![id, sid],
            )?;
            Ok(())
        })
    }
    /// Only call after authenticating the provider request and its bound call ID.
    /// Connected is deliberately not a provider status: acceptance is separate.
    pub fn phone_provider_event(
        &self,
        id: &str,
        sid: &str,
        state: CallState,
        now_ms: i64,
    ) -> PhoneResult<()> {
        self.phone_provider_event_with_usage(id, sid, state, now_ms, None)
    }

    pub fn phone_usage(&self, id: &str) -> PhoneResult<Option<ProviderUsage>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT duration_seconds,ended_ms FROM phone_call_usage WHERE call_id=?1",
                [id],
                |r| {
                    Ok(ProviderUsage {
                        duration_seconds: r.get(0)?,
                        ended_ms: r.get(1)?,
                    })
                },
            )
            .optional()
        })
        .map_err(PhoneError::from)
    }

    pub fn phone_provider_event_with_usage(
        &self,
        id: &str,
        sid: &str,
        state: CallState,
        now_ms: i64,
        usage: Option<ProviderUsage>,
    ) -> PhoneResult<()> {
        self.phone_provider_event_usage(id, sid, state, now_ms, usage, false)
    }
    /// Apply provider-confirmed nonanswer only if no answer was recorded in
    /// the same transaction. A concurrent answer callback must not refund usage.
    pub fn phone_confirm_never_answered(&self, id: &str, sid: &str, now_ms: i64) -> PhoneResult<()> {
        self.phone_provider_event_usage(id, sid, CallState::Canceled, now_ms,
            Some(ProviderUsage { duration_seconds: 0, ended_ms: now_ms }), true)
    }
    fn phone_provider_event_usage(
        &self, id: &str, sid: &str, state: CallState, now_ms: i64,
        usage: Option<ProviderUsage>, require_unanswered: bool,
    ) -> PhoneResult<()> {
        if let Some(usage) = usage {
            if !state.terminal()
                || usage.duration_seconds < 0
                || usage.duration_seconds > i64::MAX / 1000
                || usage.ended_ms < usage.duration_seconds * 1000
                || usage.ended_ms > now_ms.saturating_add(300_000)
            {
                return Err(PhoneError::InvalidInput);
            }
        }
        if matches!(
            state,
            CallState::Requesting | CallState::Uncertain | CallState::Connected
        ) || now_ms < 0
        {
            return Err(PhoneError::InvalidInput);
        }
        identifier(sid)?;
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if require_unanswered && (call.answered_ms.is_some() || call.provider_sid.as_deref() != Some(sid)) {
                return Err(PhoneError::Conflict);
            }
            if call.provider_sid.as_deref().is_some_and(|old| old != sid) {
                return Err(PhoneError::Conflict);
            }
            if let Some(usage) = usage {
                if usage.ended_ms - usage.duration_seconds * 1000
                    < call.created_ms.saturating_sub(300_000)
                {
                    return Err(PhoneError::InvalidInput);
                }
                let old: Option<ProviderUsage> = tx
                    .query_row(
                        "SELECT duration_seconds,ended_ms FROM phone_call_usage WHERE call_id=?1",
                        [id],
                        |r| {
                            Ok(ProviderUsage {
                                duration_seconds: r.get(0)?,
                                ended_ms: r.get(1)?,
                            })
                        },
                    )
                    .optional()?;
                if old.is_some_and(|old| old != usage) {
                    return Err(PhoneError::Conflict);
                }
                tx.execute("INSERT INTO phone_call_usage(call_id,duration_seconds,ended_ms) VALUES(?1,?2,?3) ON CONFLICT(call_id) DO NOTHING",params![id,usage.duration_seconds,usage.ended_ms])?;
            }
            tx.execute(
                "UPDATE phone_calls SET provider_sid=?2 WHERE id=?1",
                params![id, sid],
            )?;
            if call.state.terminal() || state.rank() < call.state.rank() {
                return Ok(());
            }
            let answered = if state == CallState::AwaitingAcceptance {
                Some(now_ms)
            } else {
                call.answered_ms
            };
            tx.execute("UPDATE phone_calls SET state=?2,answered_ms=COALESCE(answered_ms,?3),ended_ms=?4 WHERE id=?1",
                params![id,state.as_str(),answered,if state.terminal(){Some(now_ms)}else{None}])?;
            Ok(())
        })
    }
    pub fn phone_remove_destination(&self, owner: &str, now_ms: i64) -> PhoneResult<Vec<String>> {
        identifier(owner)?;
        transaction(self, |tx| {
            tx.execute("INSERT INTO phone_destinations(owner,number,updated_ms,provenance) VALUES(?1,NULL,?2,'removed') ON CONFLICT(owner) DO UPDATE SET number=NULL,updated_ms=?2,provenance='removed'", params![owner,now_ms])?;
            tx.execute("DELETE FROM phone_verifications WHERE owner=?1", [owner])?;
            tx.execute("DELETE FROM phone_setup_prompts WHERE owner=?1", [owner])?;
            tx.execute(
                "UPDATE phone_calls SET stop_requested=1 WHERE owner=?1 AND ended_ms IS NULL",
                [owner],
            )?;
            let mut stmt=tx.prepare("SELECT id FROM phone_calls WHERE owner=?1 AND ended_ms IS NULL ORDER BY created_ms")?;
            let calls = stmt
                .query_map([owner], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(calls)
        })
    }
}

impl Store {
    pub fn phone_accept(&self, id: &str, sid: &str, now_ms: i64) -> PhoneResult<bool> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.provider_sid.as_deref() != Some(sid) || call.stop_requested || call.verification
            {
                return Err(PhoneError::Conflict);
            }
            if call.state == CallState::Connected {
                return Ok(false);
            }
            if call.state != CallState::AwaitingAcceptance {
                return Err(PhoneError::Conflict);
            }
            let answered = call.answered_ms.ok_or(PhoneError::Conflict)?;
            if now_ms < answered || now_ms - answered >= 15_000 {
                return Err(PhoneError::AcceptanceExpired);
            }
            tx.execute("UPDATE phone_calls SET state='connected' WHERE id=?1", [id])?;
            Ok(true)
        })
    }
    pub fn phone_request_stop(&self, owner: &str, id: &str) -> PhoneResult<PhoneCall> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.owner != owner {
                return Err(PhoneError::NotFound);
            }
            tx.execute("UPDATE phone_calls SET stop_requested=1 WHERE id=?1", [id])?;
            call_by_id(tx, id)
        })
    }
    /// Pending values and hashes stay behind the service boundary. Hashes are
    /// HMACs produced by the phone service, not unsalted six-digit-code hashes.
    pub fn phone_begin_verification(
        &self,
        input: BeginVerification<'_>,
    ) -> PhoneResult<CallReservation> {
        identifier(input.request.owner)?;
        identifier(input.request.request_id)?;
        identifier(input.request.conversation)?;
        validate_e164(input.number)?;
        if input.request.now_ms < 0 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let existing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_calls WHERE owner=?1 AND request_id=?2)",
                params![input.request.owner, input.request.request_id],
                |r| r.get(0),
            )?;
            if !existing {
                let counts: (i64,i64) = tx.query_row(
                    "SELECT COALESCE(SUM(owner=?1),0), COALESCE(SUM(destination=?2),0) FROM phone_calls WHERE verification=1 AND created_ms>?3",
                    params![input.request.owner,input.number,input.request.now_ms-3_600_000], |r|Ok((r.get(0)?,r.get(1)?)))?;
                if counts.0 >= 3 || counts.1 >= 3 {
                    return Err(PhoneError::RateLimited);
                }
            }
            let owner = input.request.owner;
            let expires = input.request.now_ms + 300_000;
            let reserved = reserve(tx, input.request, Some(input.number), true)?;
            if reserved.should_dial {
                tx.execute("INSERT INTO phone_verifications(owner,call_id,number,code_hash,expires_ms,attempts) VALUES(?1,?2,?3,?4,?5,0) ON CONFLICT(owner) DO UPDATE SET call_id=?2,number=?3,code_hash=?4,expires_ms=?5,attempts=0",
                    params![owner,reserved.call.id,input.number,input.code_hash.as_slice(),expires])?;
            }
            Ok(reserved)
        })
    }
    pub fn phone_verification_request(&self, owner: &str) -> PhoneResult<Option<String>> {
        Ok(self.with_conn(|c| c.query_row("SELECT c.request_id FROM phone_verifications v JOIN phone_calls c ON c.id=v.call_id WHERE v.owner=?1", [owner], |r|r.get(0)).optional())?)
    }
    pub fn phone_verify_destination(
        &self,
        owner: &str,
        candidate_hash: &[u8; 32],
        now_ms: i64,
    ) -> PhoneResult<bool> {
        transaction(self, |tx| {
            let row: Option<(String,Vec<u8>,i64,i64)> = tx.query_row(
                "SELECT number,code_hash,expires_ms,attempts FROM phone_verifications WHERE owner=?1", [owner],
                |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let Some((number, hash, expires, attempts)) = row else {
                return Ok(false);
            };
            if now_ms >= expires || attempts >= 5 {
                tx.execute("DELETE FROM phone_verifications WHERE owner=?1", [owner])?;
                return Ok(false);
            }
            tx.execute(
                "UPDATE phone_verifications SET attempts=attempts+1 WHERE owner=?1",
                [owner],
            )?;
            let difference = hash
                .iter()
                .zip(candidate_hash)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b));
            if hash.len() != candidate_hash.len() || difference != 0 {
                return Ok(false);
            }
            tx.execute("INSERT INTO phone_destinations(owner,number,updated_ms,provenance) VALUES(?1,?2,?3,'chat_verified') ON CONFLICT(owner) DO UPDATE SET number=?2,updated_ms=?3,provenance='chat_verified'", params![owner,number,now_ms])?;
            tx.execute("DELETE FROM phone_verifications WHERE owner=?1", [owner])?;
            Ok(true)
        })
    }
}

impl Store {
    pub fn phone_create_rejected(&self, id: &str, now_ms: i64) -> PhoneResult<()> {
        self.with_conn(|c|c.execute("UPDATE phone_calls SET state='failed',ended_ms=?2 WHERE id=?1 AND provider_sid IS NULL AND state='requesting'",params![id,now_ms]))?;
        Ok(())
    }
}

impl Store {
    /// Trusted provider ingress only, after verifying the callback signature.
    pub fn phone_provider_call(&self, id: &str) -> PhoneResult<PhoneCall> {
        transaction(self, |tx| call_by_id(tx, id))
    }
}

impl Store {
    pub fn phone_claim_relay(&self, id: &str, sid: &str) -> PhoneResult<PhoneCall> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.state != CallState::Connected
                || call.stop_requested
                || call.verification
                || call.provider_sid.as_deref() != Some(sid)
            {
                return Err(PhoneError::Conflict);
            }
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO phone_relay_claims(call_id) VALUES(?1)",
                [id],
            )?;
            if inserted != 1 {
                return Err(PhoneError::Conflict);
            }
            Ok(call)
        })
    }
    pub fn phone_active_calls(&self) -> PhoneResult<Vec<PhoneCall>> {
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM phone_calls WHERE ended_ms IS NULL ORDER BY created_ms"
            ))?;
            let calls = stmt
                .query_map([], read_call)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(calls)
        })?)
    }
    /// Bounded candidates for authenticated REST recovery of missing final usage.
    pub fn phone_usage_reconciliation_due(&self, now_ms: i64) -> PhoneResult<Vec<PhoneCall>> {
        if now_ms < 0 { return Err(PhoneError::InvalidInput); }
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM phone_calls WHERE ended_ms IS NOT NULL AND provider_sid IS NOT NULL
                 AND NOT EXISTS(SELECT 1 FROM phone_call_usage WHERE call_id=phone_calls.id)
                 AND COALESCE((SELECT attempts FROM phone_usage_reconciliation WHERE call_id=phone_calls.id),0)<3
                 AND COALESCE((SELECT next_ms FROM phone_usage_reconciliation WHERE call_id=phone_calls.id),0)<=?1
                 ORDER BY ended_ms LIMIT 8"))?;
            let calls = stmt.query_map([now_ms], read_call)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(calls)
        })?)
    }
    pub fn phone_claim_usage_reconciliation(&self, id: &str, now_ms: i64) -> PhoneResult<bool> {
        if now_ms < 0 { return Err(PhoneError::InvalidInput); }
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if !call.state.terminal() || call.provider_sid.is_none() { return Ok(false); }
            let finalized: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_call_usage WHERE call_id=?1)", [id], |r| r.get(0))?;
            if finalized { return Ok(false); }
            tx.execute("INSERT OR IGNORE INTO phone_usage_reconciliation(call_id) VALUES(?1)", [id])?;
            let (attempts, next): (i64, i64) = tx.query_row("SELECT attempts,next_ms FROM phone_usage_reconciliation WHERE call_id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if attempts >= 3 || now_ms < next { return Ok(false); }
            tx.execute("UPDATE phone_usage_reconciliation SET attempts=attempts+1,next_ms=?2 WHERE call_id=?1", params![id, now_ms.saturating_add(1000 * (1 << attempts))])?;
            Ok(true)
        })
    }

    pub fn phone_claim_cleanup(&self, id: &str, now_ms: i64) -> PhoneResult<bool> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if !call.stop_requested || call.state.terminal() || call.provider_sid.is_none() {
                return Ok(false);
            }
            tx.execute(
                "INSERT OR IGNORE INTO phone_cleanup(call_id) VALUES(?1)",
                [id],
            )?;
            let (attempts, next): (i64, i64) = tx.query_row(
                "SELECT attempts,next_ms FROM phone_cleanup WHERE call_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if attempts >= 3 || now_ms < next {
                return Ok(false);
            }
            tx.execute(
                "UPDATE phone_cleanup SET attempts=attempts+1,next_ms=?2 WHERE call_id=?1",
                params![id, now_ms + 1000 * (1 << attempts)],
            )?;
            Ok(true)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhoneOrigin {
    pub owner: crate::SurfaceOwnerRef,
    pub conversation: crate::SurfaceConversationRef,
}
impl Store {
    pub fn phone_register_origin(
        &self,
        id: &str,
        origin: &PhoneOrigin,
        revision: i64,
    ) -> PhoneResult<()> {
        if origin.owner.account() != origin.conversation.account() {
            return Err(PhoneError::Conflict);
        }
        let json = serde_json::to_string(origin).map_err(|_| PhoneError::InvalidInput)?;
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.conversation != origin.conversation.storage_key() {
                return Err(PhoneError::Conflict);
            }
            let current: Option<i64> = tx.query_row(
                "SELECT r.revision FROM surface_authority_revisions r JOIN surface_owner_bindings b
                 USING(platform,account_id) WHERE r.platform=?1 AND r.account_id=?2 AND b.owner_sender_id=?3",
                params![origin.owner.account().platform().as_str(), origin.owner.account().account_id(), origin.owner.sender_id()],
                |r| r.get(0),
            ).optional()?;
            if current != Some(revision) {
                return Err(PhoneError::Conflict);
            }
            let saved: Option<i64> = tx
                .query_row(
                    "SELECT revision FROM phone_origin_authority WHERE call_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if saved.is_some_and(|old| old != revision) {
                return Err(PhoneError::Conflict);
            }
            let existing: Option<String> = tx
                .query_row(
                    "SELECT origin_json FROM phone_origins WHERE call_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.as_ref().is_some_and(|old| old != &json) {
                return Err(PhoneError::Conflict);
            }
            tx.execute(
                "INSERT OR IGNORE INTO phone_origins(call_id,origin_json) VALUES(?1,?2)",
                params![id, json],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO phone_origin_authority(call_id,revision) VALUES(?1,?2)",
                params![id, revision],
            )?;
            Ok(())
        })
    }
    pub fn phone_origin_revision(&self, id: &str) -> PhoneResult<i64> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT revision FROM phone_origin_authority WHERE call_id=?1",
                [id],
                |r| r.get(0),
            )
        })
        .map_err(PhoneError::from)
    }
    pub fn phone_origin(&self, id: &str) -> PhoneResult<PhoneOrigin> {
        let json: Option<String> = self.with_conn(|c| {
            c.query_row(
                "SELECT origin_json FROM phone_origins WHERE call_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
        })?;
        serde_json::from_str(&json.ok_or(PhoneError::NotFound)?)
            .map_err(|_| PhoneError::InvalidInput)
    }
}

impl Store {
    pub fn phone_latest_call(&self, owner: &str) -> PhoneResult<Option<PhoneCall>> {
        Ok(self.with_conn(|c|c.query_row(&format!("SELECT {CALL_COLUMNS} FROM phone_calls WHERE owner=?1 ORDER BY created_ms DESC,id DESC LIMIT 1"),[owner],read_call).optional())?)
    }
}

/// Contains no destination, provider SID, transcript or verification material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhoneMaintenanceStatus {
    pub provider_sid_known: bool,
    pub cleanup_attempts: i64,
    pub usage_attempts: i64,
    pub final_usage_recorded: bool,
}

/// Which model layer answered the turn (#1546 adds the fast layer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnProvider {
    Fast,
    Native,
}
impl TurnProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Native => "native",
        }
    }
    fn parse(s: &str) -> rusqlite::Result<Self> {
        Ok(match s {
            "fast" => Self::Fast,
            "native" => Self::Native,
            _ => return Err(rusqlite::Error::InvalidQuery),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Answered,
    Interrupted,
    Failed,
    Dispatched,
}
impl TurnOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Dispatched => "dispatched",
        }
    }
    fn parse(s: &str) -> rusqlite::Result<Self> {
        Ok(match s {
            "answered" => Self::Answered,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            "dispatched" => Self::Dispatched,
            _ => return Err(rusqlite::Error::InvalidQuery),
        })
    }
}
/// One turn's latency record (#1551). Millisecond offsets are relative to the
/// receipt of the final prompt frame. The only strings are call/turn IDs,
/// closed enums and a Git commit SHA; speech text is never stored here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhoneTurnMetrics {
    pub call_id: String,
    pub turn_id: String,
    pub recorded_ms: i64,
    pub provider: TurnProvider,
    pub outcome: TurnOutcome,
    pub queue_wait_ms: Option<i64>,
    pub t_first_token_ms: Option<i64>,
    pub t_first_speech_ms: Option<i64>,
    pub t_last_speech_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub t_first_played_ms: Option<i64>,
    pub t_last_played_ms: Option<i64>,
    pub audible_delta_ms: Option<i64>,
    pub commit_sha: Option<String>,
    /// #1555: a speculation was started on an interim transcript.
    pub speculated: bool,
    /// #1555: the final transcript committed that speculation's answer.
    pub speculation_committed: bool,
    /// #1555: final-prompt time minus speculation start, when committed.
    pub speculation_saved_ms: Option<i64>,
}
/// Percentile columns, in CLI display order.
pub const PHONE_TURN_METRIC_FIELDS: [&str; 9] = [
    "queue_wait_ms",
    "t_first_token_ms",
    "t_first_speech_ms",
    "t_last_speech_ms",
    "tool_ms",
    "t_first_played_ms",
    "t_last_played_ms",
    "audible_delta_ms",
    "speculation_saved_ms",
];
impl PhoneTurnMetrics {
    pub fn field(&self, name: &str) -> Option<Option<i64>> {
        Some(match name {
            "queue_wait_ms" => self.queue_wait_ms,
            "t_first_token_ms" => self.t_first_token_ms,
            "t_first_speech_ms" => self.t_first_speech_ms,
            "t_last_speech_ms" => self.t_last_speech_ms,
            "tool_ms" => self.tool_ms,
            "t_first_played_ms" => self.t_first_played_ms,
            "t_last_played_ms" => self.t_last_played_ms,
            "audible_delta_ms" => self.audible_delta_ms,
            "speculation_saved_ms" => self.speculation_saved_ms,
            _ => return None,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhoneMetricPercentiles {
    pub field: &'static str,
    pub samples: usize,
    pub p50: Option<i64>,
    pub p95: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhoneMetricsSummary {
    pub calls: usize,
    pub turns: usize,
    pub fields: Vec<PhoneMetricPercentiles>,
}
/// Nearest-rank percentile: the ceil(p/100 * n)-th smallest sample.
pub fn nearest_rank_percentile(sorted: &[i64], percent: u32) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() as u64 * u64::from(percent)).div_ceil(100).max(1) as usize;
    sorted.get(rank.min(sorted.len()) - 1).copied()
}
const METRIC_COLUMNS: &str = "call_id,turn_id,recorded_ms,provider,outcome,queue_wait_ms,t_first_token_ms,t_first_speech_ms,t_last_speech_ms,tool_ms,t_first_played_ms,t_last_played_ms,audible_delta_ms,commit_sha,speculated,speculation_committed,speculation_saved_ms";
fn read_metrics(row: &rusqlite::Row<'_>) -> rusqlite::Result<PhoneTurnMetrics> {
    Ok(PhoneTurnMetrics {
        call_id: row.get(0)?,
        turn_id: row.get(1)?,
        recorded_ms: row.get(2)?,
        provider: TurnProvider::parse(&row.get::<_, String>(3)?)?,
        outcome: TurnOutcome::parse(&row.get::<_, String>(4)?)?,
        queue_wait_ms: row.get(5)?,
        t_first_token_ms: row.get(6)?,
        t_first_speech_ms: row.get(7)?,
        t_last_speech_ms: row.get(8)?,
        tool_ms: row.get(9)?,
        t_first_played_ms: row.get(10)?,
        t_last_played_ms: row.get(11)?,
        audible_delta_ms: row.get(12)?,
        commit_sha: row.get(13)?,
        speculated: row.get(14)?,
        speculation_committed: row.get(15)?,
        speculation_saved_ms: row.get(16)?,
    })
}
impl Store {
    /// Insert one latency record. Returns false when the turn already has one;
    /// a replayed turn never produces a second row.
    pub fn phone_record_turn_metrics(&self, m: &PhoneTurnMetrics) -> PhoneResult<bool> {
        identifier(&m.call_id)?;
        identifier(&m.turn_id)?;
        if m.recorded_ms < 0
            || m.turn_id.chars().any(char::is_whitespace)
            || m.call_id.chars().any(char::is_whitespace)
        {
            return Err(PhoneError::InvalidInput);
        }
        for name in PHONE_TURN_METRIC_FIELDS {
            if name != "audible_delta_ms" && m.field(name).flatten().is_some_and(|v| v < 0) {
                return Err(PhoneError::InvalidInput);
            }
        }
        // A committed speculation was started; only a committed one saved time.
        if (m.speculation_committed && !m.speculated)
            || m.speculation_saved_ms.is_some() != m.speculation_committed
        {
            return Err(PhoneError::InvalidInput);
        }
        if let Some(sha) = &m.commit_sha {
            if !(7..=40).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(PhoneError::InvalidInput);
            }
        }
        Ok(self.with_conn(|c| c.execute(
            &format!("INSERT INTO phone_turn_metrics({METRIC_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) ON CONFLICT(call_id,turn_id) DO NOTHING"),
            params![m.call_id, m.turn_id, m.recorded_ms, m.provider.as_str(), m.outcome.as_str(), m.queue_wait_ms, m.t_first_token_ms, m.t_first_speech_ms, m.t_last_speech_ms, m.tool_ms, m.t_first_played_ms, m.t_last_played_ms, m.audible_delta_ms, m.commit_sha, m.speculated, m.speculation_committed, m.speculation_saved_ms]))? == 1)
    }
    pub fn phone_turn_metrics(&self, call_id: &str) -> PhoneResult<Vec<PhoneTurnMetrics>> {
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {METRIC_COLUMNS} FROM phone_turn_metrics WHERE call_id=?1 ORDER BY recorded_ms,turn_id"
            ))?;
            let rows = stmt
                .query_map([call_id], read_metrics)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?)
    }
    /// Nearest-rank p50/p95 per field over the turns of the `last_calls` most
    /// recently recorded calls. Null fields are excluded from their sample set.
    pub fn phone_turn_metrics_summary(&self, last_calls: usize) -> PhoneResult<PhoneMetricsSummary> {
        let records = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {METRIC_COLUMNS} FROM phone_turn_metrics WHERE call_id IN (
                    SELECT call_id FROM phone_turn_metrics GROUP BY call_id
                    ORDER BY MAX(recorded_ms) DESC, call_id DESC LIMIT ?1)
                 ORDER BY recorded_ms,turn_id"
            ))?;
            let rows = stmt
                .query_map([i64::try_from(last_calls).unwrap_or(i64::MAX)], read_metrics)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let calls = records
            .iter()
            .map(|r| r.call_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let fields = PHONE_TURN_METRIC_FIELDS
            .iter()
            .map(|field| {
                let mut samples: Vec<i64> = records
                    .iter()
                    .filter_map(|r| r.field(field).flatten())
                    .collect();
                samples.sort_unstable();
                PhoneMetricPercentiles {
                    field,
                    samples: samples.len(),
                    p50: nearest_rank_percentile(&samples, 50),
                    p95: nearest_rank_percentile(&samples, 95),
                }
            })
            .collect();
        Ok(PhoneMetricsSummary {
            calls,
            turns: records.len(),
            fields,
        })
    }
}

/// #1552 bounds, enforced again here so no caller can exceed them.
pub const PHONE_SUMMARY_MAX_CHARS: usize = 800;
pub const PHONE_PROFILE_MAX_FACTS: usize = 10;
pub const PHONE_PROFILE_KEY_MAX: usize = 64;
pub const PHONE_PROFILE_VALUE_MAX: usize = 200;
/// The profile key whose value is the owner's first name.
pub const PHONE_PROFILE_FIRST_NAME: &str = "first_name";
/// A claimed summary job is not due again for this long (doubling per attempt),
/// longer than one bounded summarization run.
const SUMMARY_LEASE_MS: i64 = 600_000;

/// A profile fact as produced by one summarization run.
#[derive(Clone, Debug, PartialEq)]
pub struct NewProfileFact {
    pub key: String,
    pub value: String,
    pub confidence: f64,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProfileFact {
    pub key: String,
    pub value: String,
    pub confidence: f64,
    pub source_call_id: String,
    pub updated_ms: i64,
}
/// Lowercase ASCII letters, digits, `_`, `.` and `-`; at most 64 bytes.
pub fn valid_profile_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= PHONE_PROFILE_KEY_MAX
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}
fn valid_fact(fact: &NewProfileFact) -> bool {
    valid_profile_key(&fact.key)
        && !fact.value.trim().is_empty()
        && fact.value.chars().count() <= PHONE_PROFILE_VALUE_MAX
        && fact.confidence.is_finite()
        && (0.0..=1.0).contains(&fact.confidence)
}

impl Store {
    /// Enqueue the post-call summary job. Only calls that reached a relay
    /// session have a transcript; verification calls never do. Returns whether
    /// a new job was created; duplicate callbacks and replays return false.
    pub fn phone_enqueue_call_summary(&self, call_id: &str, now_ms: i64) -> PhoneResult<bool> {
        transaction(self, |tx| {
            let call = call_by_id(tx, call_id)?;
            let relayed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_relay_claims WHERE call_id=?1)",
                [call_id],
                |r| r.get(0),
            )?;
            if call.verification || !relayed {
                return Ok(false);
            }
            Ok(tx.execute(
                "INSERT OR IGNORE INTO phone_call_summary_jobs(call_id,created_ms) VALUES(?1,?2)",
                params![call_id, now_ms],
            )? == 1)
        })
    }
    /// Pending jobs whose lease has expired. The relay session has ended, so
    /// the transcript is complete even before the final status callback.
    pub fn phone_call_summaries_due(&self, now_ms: i64) -> PhoneResult<Vec<PhoneCall>> {
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CALL_COLUMNS} FROM phone_calls WHERE id IN (
                    SELECT call_id FROM phone_call_summary_jobs
                    WHERE state='pending' AND attempts<3 AND next_ms<=?1 AND NOT EXISTS(SELECT 1 FROM phone_live_history_gaps g WHERE g.call_id=phone_call_summary_jobs.call_id))
                 ORDER BY created_ms, id LIMIT 4"
            ))?;
            let calls = stmt
                .query_map([now_ms], read_call)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(calls)
        })?)
    }
    /// Take a durable lease on one job attempt. False when the job is done,
    /// out of attempts, or still leased by an earlier attempt.
    pub fn phone_claim_call_summary(&self, call_id: &str, now_ms: i64) -> PhoneResult<bool> {
        transaction(self, |tx| {
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_live_history_gaps WHERE call_id=?1)", [call_id], |r| r.get::<_,bool>(0))? { return Ok(false); }
            let job: Option<(String, i64, i64)> = tx
                .query_row(
                    "SELECT state,attempts,next_ms FROM phone_call_summary_jobs WHERE call_id=?1",
                    [call_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((state, attempts, next)) = job else {
                return Ok(false);
            };
            if state != "pending" || attempts >= 3 || now_ms < next {
                return Ok(false);
            }
            tx.execute(
                "UPDATE phone_call_summary_jobs SET attempts=attempts+1,next_ms=?2 WHERE call_id=?1",
                params![call_id, now_ms.saturating_add(SUMMARY_LEASE_MS << attempts)],
            )?;
            Ok(true)
        })
    }
    /// Persist one run's result and mark the job done, atomically. A job that
    /// is already done is left untouched (returns false), so a replayed run
    /// never changes stored rows.
    pub fn phone_complete_call_summary(
        &self,
        call_id: &str,
        summary: &str,
        wiki_path: &str,
        facts: &[NewProfileFact],
        now_ms: i64,
    ) -> PhoneResult<bool> {
        if summary.chars().count() > PHONE_SUMMARY_MAX_CHARS
            || facts.len() > PHONE_PROFILE_MAX_FACTS
            || wiki_path.is_empty()
            || wiki_path.len() > 256
            || !facts.iter().all(valid_fact)
            || facts
                .iter()
                .enumerate()
                .any(|(i, f)| facts[..i].iter().any(|old| old.key == f.key))
        {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_live_history_gaps WHERE call_id=?1)", [call_id], |r| r.get::<_,bool>(0))? { return Ok(false); }

            let call = call_by_id(tx, call_id)?;
            let pending: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_call_summary_jobs WHERE call_id=?1 AND state='pending')",
                [call_id],
                |r| r.get(0),
            )?;
            if !pending {
                return Ok(false);
            }
            for (seq, fact) in facts.iter().enumerate() {
                tx.execute(
                    "INSERT INTO owner_profile(owner,key,value,confidence,source_call_id,updated_ms,seq)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(owner,key) DO UPDATE SET value=excluded.value,
                        confidence=excluded.confidence,source_call_id=excluded.source_call_id,
                        updated_ms=excluded.updated_ms,seq=excluded.seq",
                    params![call.owner, fact.key, fact.value.trim(), fact.confidence, call_id, now_ms, seq as i64],
                )?;
            }
            tx.execute(
                "UPDATE phone_call_summary_jobs SET state='done',done_ms=?2,summary=?3,wiki_path=?4 WHERE call_id=?1",
                params![call_id, now_ms, summary, wiki_path],
            )?;
            Ok(true)
        })
    }
    /// The owner's facts, most recently learned first; within one call, in
    /// the order the summarizer produced them.
    pub fn phone_owner_profile(&self, owner: &str) -> PhoneResult<Vec<ProfileFact>> {
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key,value,confidence,source_call_id,updated_ms FROM owner_profile
                 WHERE owner=?1 ORDER BY updated_ms DESC, seq ASC, key ASC",
            )?;
            let facts = stmt
                .query_map([owner], |r| {
                    Ok(ProfileFact {
                        key: r.get(0)?,
                        value: r.get(1)?,
                        confidence: r.get(2)?,
                        source_call_id: r.get(3)?,
                        updated_ms: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(facts)
        })?)
    }
    pub fn phone_owner_first_name(&self, owner: &str) -> PhoneResult<Option<String>> {
        Ok(self.with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM owner_profile WHERE owner=?1 AND key=?2",
                params![owner, PHONE_PROFILE_FIRST_NAME],
                |r| r.get(0),
            )
            .optional()
        })?)
    }
    /// Returns whether a fact was removed.
    pub fn phone_forget_profile_fact(&self, owner: &str, key: &str) -> PhoneResult<bool> {
        Ok(self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM owner_profile WHERE owner=?1 AND key=?2",
                params![owner, key],
            )
        })? > 0)
    }
    /// Stored summaries of the owner's most recent summarized calls, newest first.
    pub fn phone_recent_call_summaries(&self, owner: &str, limit: usize) -> PhoneResult<Vec<String>> {
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT j.summary FROM phone_call_summary_jobs j JOIN phone_calls c ON c.id=j.call_id
                 WHERE c.owner=?1 AND j.state='done' AND j.summary IS NOT NULL AND j.summary<>''
                 ORDER BY c.created_ms DESC, c.id DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![owner, i64::try_from(limit).unwrap_or(i64::MAX)], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?)
    }
}

/// Why a relayed call ended, as recorded from the relay-ended callback (#1557).
pub const PHONE_END_REASONS: [&str; 5] = [
    "owner_goodbye",
    "task_complete",
    "owner_request",
    "relay_ended_unknown",
    // #1556: recorded by the relay itself, with a chat notice.
    "silence",
];
impl Store {
    /// Record a call's end reason. First write wins, so a replayed callback
    /// never changes it. Returns whether a row was written.
    pub fn phone_record_end_reason(
        &self,
        call_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> PhoneResult<bool> {
        if !PHONE_END_REASONS.contains(&reason) {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            call_by_id(tx, call_id)?;
            Ok(tx.execute(
                "INSERT OR IGNORE INTO phone_call_end_reasons(call_id,reason,recorded_ms) VALUES(?1,?2,?3)",
                params![call_id, reason, now_ms],
            )? == 1)
        })
    }
    /// The recorded end reason; `None` when the call ended without one.
    pub fn phone_end_reason(&self, call_id: &str) -> PhoneResult<Option<String>> {
        Ok(self.with_conn(|conn| {
            conn.query_row(
                "SELECT reason FROM phone_call_end_reasons WHERE call_id=?1",
                [call_id],
                |r| r.get(0),
            )
            .optional()
        })?)
    }
}

// ---- #1553 agent-proposed calls -------------------------------------------

/// A proposal waits this long for the owner's answer after it is posted.
pub const PHONE_PROPOSAL_TTL_MS: i64 = 15 * 60_000;
/// At most one proposal per owner within this distance of another.
pub const PHONE_PROPOSAL_SPACING_MS: i64 = 4 * 60 * 60_000;
/// At most this many proposals per owner in any rolling 24 hours.
pub const PHONE_PROPOSALS_PER_DAY: i64 = 3;
/// Bullets carried into the first phone turn.
pub const PHONE_PROPOSAL_AGENDA_MAX: usize = 5;
pub const PHONE_PROPOSAL_REASON_MAX: usize = 300;
pub const PHONE_PROPOSAL_ITEM_MAX: usize = 200;
/// A scheduled proposal not posted within this long after its delivery time
/// (no chat adapter, repeated delivery failure) expires instead of arriving late.
pub const PHONE_PROPOSAL_STALE_MS: i64 = 60 * 60_000;
const DAY_MS: i64 = 24 * 60 * 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalState {
    Scheduled,
    Pending,
    Accepted,
    Declined,
    Expired,
}
impl ProposalState {
    fn parse(s: &str) -> rusqlite::Result<Self> {
        Ok(match s {
            "scheduled" => Self::Scheduled,
            "pending" => Self::Pending,
            "accepted" => Self::Accepted,
            "declined" => Self::Declined,
            "expired" => Self::Expired,
            _ => return Err(rusqlite::Error::InvalidQuery),
        })
    }
}
#[derive(Clone, Debug)]
pub struct CallProposal {
    pub id: String,
    pub owner: String,
    pub origin: PhoneOrigin,
    pub reason: String,
    pub agenda: Vec<String>,
    pub state: ProposalState,
    pub created_ms: i64,
    pub deliver_ms: i64,
    pub delivered_ms: Option<i64>,
    pub expires_ms: Option<i64>,
    /// `call_owner` request ID used by the accepted proposal's call.
    pub call_request: Option<String>,
}
pub struct NewCallProposal<'a> {
    pub owner: &'a str,
    pub origin: &'a PhoneOrigin,
    pub reason: &'a str,
    pub agenda: &'a [String],
    pub now_ms: i64,
    /// `now_ms`, or the end of the owner's quiet hours.
    pub deliver_ms: i64,
}
/// Result of an owner's yes/no in a control conversation.
#[derive(Debug)]
pub enum ProposalAnswer {
    /// Newly accepted: the caller dials once with `call_request`.
    Accepted(CallProposal),
    /// A duplicate yes: the call was already requested.
    AlreadyAccepted(CallProposal),
    Declined,
    AlreadyDeclined,
    Expired,
    /// The open proposal was posted in a different conversation.
    OtherConversation,
    NoProposal,
}
const PROPOSAL_COLUMNS: &str = "id,owner,origin_json,reason,agenda_json,state,created_ms,deliver_ms,delivered_ms,expires_ms,call_request";
fn read_proposal(r: &rusqlite::Row<'_>) -> rusqlite::Result<CallProposal> {
    let bad = |_| rusqlite::Error::InvalidQuery;
    Ok(CallProposal {
        id: r.get(0)?,
        owner: r.get(1)?,
        origin: serde_json::from_str(&r.get::<_, String>(2)?).map_err(bad)?,
        reason: r.get(3)?,
        agenda: serde_json::from_str(&r.get::<_, String>(4)?).map_err(bad)?,
        state: ProposalState::parse(&r.get::<_, String>(5)?)?,
        created_ms: r.get(6)?,
        deliver_ms: r.get(7)?,
        delivered_ms: r.get(8)?,
        expires_ms: r.get(9)?,
        call_request: r.get(10)?,
    })
}
fn bounded_line(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

impl Store {
    /// Record a proposal after the rate limits: no other proposal for the
    /// owner within [`PHONE_PROPOSAL_SPACING_MS`] of its delivery time and
    /// fewer than [`PHONE_PROPOSALS_PER_DAY`] delivered in the 24 hours before
    /// it. A rejected proposal is not recorded.
    pub fn phone_schedule_call_proposal(&self, p: NewCallProposal<'_>) -> PhoneResult<CallProposal> {
        identifier(p.owner)?;
        if !bounded_line(p.reason, PHONE_PROPOSAL_REASON_MAX)
            || p.agenda.len() > PHONE_PROPOSAL_AGENDA_MAX
            || !p.agenda.iter().all(|item| bounded_line(item, PHONE_PROPOSAL_ITEM_MAX))
            || p.deliver_ms < p.now_ms
            || p.origin.owner.account() != p.origin.conversation.account()
        {
            return Err(PhoneError::InvalidInput);
        }
        let origin = serde_json::to_string(p.origin).map_err(|_| PhoneError::InvalidInput)?;
        let agenda = serde_json::to_string(p.agenda).map_err(|_| PhoneError::InvalidInput)?;
        let id = uuid::Uuid::new_v4().to_string();
        transaction(self, |tx| {
            let near: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_call_proposals
                 WHERE owner=?1 AND deliver_ms>?2 AND deliver_ms<?3)",
                params![p.owner, p.deliver_ms - PHONE_PROPOSAL_SPACING_MS, p.deliver_ms + PHONE_PROPOSAL_SPACING_MS],
                |r| r.get(0),
            )?;
            let today: i64 = tx.query_row(
                "SELECT COUNT(*) FROM phone_call_proposals
                 WHERE owner=?1 AND deliver_ms>?2 AND deliver_ms<=?3",
                params![p.owner, p.deliver_ms - DAY_MS, p.deliver_ms],
                |r| r.get(0),
            )?;
            if near || today >= PHONE_PROPOSALS_PER_DAY {
                return Err(PhoneError::RateLimited);
            }
            tx.execute(
                "INSERT INTO phone_call_proposals(id,owner,conversation,origin_json,reason,agenda_json,state,created_ms,deliver_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,'scheduled',?7,?8)",
                params![id, p.owner, p.origin.conversation.storage_key(), origin, p.reason.trim(), agenda, p.now_ms, p.deliver_ms],
            )?;
            Ok(tx.query_row(
                &format!("SELECT {PROPOSAL_COLUMNS} FROM phone_call_proposals WHERE id=?1"),
                [&id],
                read_proposal,
            )?)
        })
    }
    /// Scheduled proposals whose delivery time has come. Ones left unposted
    /// for longer than [`PHONE_PROPOSAL_STALE_MS`] expire here instead.
    pub fn phone_call_proposals_due(&self, now_ms: i64) -> PhoneResult<Vec<CallProposal>> {
        transaction(self, |tx| {
            tx.execute(
                "UPDATE phone_call_proposals SET state='expired',decided_ms=?1
                 WHERE state='scheduled' AND deliver_ms<=?2",
                params![now_ms, now_ms - PHONE_PROPOSAL_STALE_MS],
            )?;
            let mut stmt = tx.prepare(&format!(
                "SELECT {PROPOSAL_COLUMNS} FROM phone_call_proposals
                 WHERE state='scheduled' AND deliver_ms<=?1 ORDER BY deliver_ms, id LIMIT 8"
            ))?;
            let due = stmt
                .query_map([now_ms], read_proposal)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(due)
        })
    }
    /// The question is in the owner's chat: start its answer window. False
    /// when it was already delivered or is no longer scheduled.
    pub fn phone_mark_call_proposal_delivered(&self, id: &str, now_ms: i64) -> PhoneResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE phone_call_proposals SET state='pending',delivered_ms=?2,expires_ms=?3
                 WHERE id=?1 AND state='scheduled'",
                params![id, now_ms, now_ms + PHONE_PROPOSAL_TTL_MS],
            )
        })? == 1)
    }
    /// Unanswered proposals past their window are recorded as expired
    /// (a decline). Returns how many expired.
    pub fn phone_expire_call_proposals(&self, now_ms: i64) -> PhoneResult<usize> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE phone_call_proposals SET state='expired',decided_ms=?1
                 WHERE state='pending' AND expires_ms<=?1",
                [now_ms],
            )
        })?)
    }
    /// Whether any posted proposal's window (plus `grace_ms` for late replies)
    /// is still open, so bare yes/no replies are routed to the phone service.
    /// An answered proposal's window ends at its decision time plus grace.
    pub fn phone_call_proposal_window_open(&self, now_ms: i64, grace_ms: i64) -> PhoneResult<bool> {
        Ok(self.with_conn(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_call_proposals
                 WHERE delivered_ms IS NOT NULL AND (CASE WHEN state IN ('accepted','declined') THEN decided_ms ELSE expires_ms END)+?2>?1)",
                params![now_ms, grace_ms],
                |r| r.get(0),
            )
        })?)
    }
    /// Apply an authenticated owner's yes/no. Only the most recently posted
    /// proposal whose window (plus `grace_ms`) is open is considered; it must
    /// have been posted in `conversation` (a storage key). Acceptance is a
    /// single transition, so duplicate replies cannot accept twice.
    pub fn phone_answer_call_proposal(
        &self,
        owner: &str,
        conversation: &str,
        accept: bool,
        now_ms: i64,
        grace_ms: i64,
    ) -> PhoneResult<ProposalAnswer> {
        transaction(self, |tx| {
            let latest = tx
                .query_row(
                    &format!(
                        "SELECT {PROPOSAL_COLUMNS},conversation FROM phone_call_proposals
                         WHERE owner=?1 AND delivered_ms IS NOT NULL AND (CASE WHEN state IN ('accepted','declined') THEN decided_ms ELSE expires_ms END)+?3>?2
                         ORDER BY delivered_ms DESC, id DESC LIMIT 1"
                    ),
                    params![owner, now_ms, grace_ms],
                    |r| Ok((read_proposal(r)?, r.get::<_, String>(11)?)),
                )
                .optional()?;
            let Some((mut proposal, posted_in)) = latest else {
                return Ok(ProposalAnswer::NoProposal);
            };
            if posted_in != conversation {
                return Ok(ProposalAnswer::OtherConversation);
            }
            if proposal.state == ProposalState::Pending
                && proposal.expires_ms.is_some_and(|at| now_ms >= at)
            {
                tx.execute(
                    "UPDATE phone_call_proposals SET state='expired',decided_ms=?2 WHERE id=?1",
                    params![proposal.id, now_ms],
                )?;
                return Ok(ProposalAnswer::Expired);
            }
            Ok(match (proposal.state, accept) {
                (ProposalState::Pending, true) => {
                    let request = format!("proposal:{}", proposal.id);
                    tx.execute(
                        "UPDATE phone_call_proposals SET state='accepted',decided_ms=?2,call_request=?3 WHERE id=?1",
                        params![proposal.id, now_ms, request],
                    )?;
                    proposal.state = ProposalState::Accepted;
                    proposal.call_request = Some(request);
                    ProposalAnswer::Accepted(proposal)
                }
                (ProposalState::Pending, false) => {
                    tx.execute(
                        "UPDATE phone_call_proposals SET state='declined',decided_ms=?2 WHERE id=?1",
                        params![proposal.id, now_ms],
                    )?;
                    ProposalAnswer::Declined
                }
                (ProposalState::Accepted, _) => ProposalAnswer::AlreadyAccepted(proposal),
                (ProposalState::Declined, _) => ProposalAnswer::AlreadyDeclined,
                _ => ProposalAnswer::Expired,
            })
        })
    }
    /// The accepted proposal that requested this call, if any.
    pub fn phone_call_proposal_for_call(&self, call_id: &str) -> PhoneResult<Option<CallProposal>> {
        Ok(self.with_conn(|c| {
            c.query_row(
                &format!(
                    "SELECT {} FROM phone_call_proposals p JOIN phone_calls c
                     ON c.owner=p.owner AND c.request_id=p.call_request
                     WHERE c.id=?1 AND p.state='accepted'",
                    PROPOSAL_COLUMNS.split(',').map(|col| format!("p.{col}")).collect::<Vec<_>>().join(",")
                ),
                [call_id],
                read_proposal,
            )
            .optional()
        })?)
    }
    /// Every proposal for an owner, oldest first. For status and tests.
    pub fn phone_call_proposals(&self, owner: &str) -> PhoneResult<Vec<CallProposal>> {
        Ok(self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {PROPOSAL_COLUMNS} FROM phone_call_proposals WHERE owner=?1 ORDER BY created_ms, id"
            ))?;
            let rows = stmt
                .query_map([owner], read_proposal)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?)
    }
}
/// First retry delay for an end notice (#1556); doubles per attempt.
const END_NOTICE_LEASE_MS: i64 = 60_000;
impl Store {
    /// Record an end the runtime chose itself (#1556) and queue its one chat
    /// notice in the same transaction. First write wins: returns false, and
    /// queues nothing, when the call already has an end reason.
    pub fn phone_record_end_with_notice(
        &self,
        call_id: &str,
        reason: &str,
        now_ms: i64,
    ) -> PhoneResult<bool> {
        if !PHONE_END_REASONS.contains(&reason) {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            call_by_id(tx, call_id)?;
            if tx.execute(
                "INSERT OR IGNORE INTO phone_call_end_reasons(call_id,reason,recorded_ms) VALUES(?1,?2,?3)",
                params![call_id, reason, now_ms],
            )? == 0
            {
                return Ok(false);
            }
            tx.execute(
                "INSERT OR IGNORE INTO phone_call_end_notices(call_id,created_ms) VALUES(?1,?2)",
                params![call_id, now_ms],
            )?;
            Ok(true)
        })
    }
    /// Calls with a pending end notice not leased by an earlier attempt,
    /// with their end reason.
    pub fn phone_end_notices_due(&self, now_ms: i64) -> PhoneResult<Vec<(PhoneCall, String)>> {
        let columns = CALL_COLUMNS
            .split(',')
            .map(|c| format!("c.{c}"))
            .collect::<Vec<_>>()
            .join(",");
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {columns}, r.reason FROM phone_call_end_notices n
                 JOIN phone_call_end_reasons r ON r.call_id=n.call_id
                 JOIN phone_calls c ON c.id=n.call_id
                 WHERE n.state='pending' AND n.attempts<3 AND n.next_ms<=?1
                 ORDER BY n.created_ms, c.id LIMIT 8"
            ))?;
            let due = stmt
                .query_map([now_ms], |row| Ok((read_call(row)?, row.get(12)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(due)
        })?)
    }
    /// Lease one delivery attempt. False when the notice was delivered, is
    /// out of attempts, or is still leased by an earlier attempt.
    pub fn phone_claim_end_notice(&self, call_id: &str, now_ms: i64) -> PhoneResult<bool> {
        transaction(self, |tx| {
            let notice: Option<(String, i64, i64)> = tx
                .query_row(
                    "SELECT state,attempts,next_ms FROM phone_call_end_notices WHERE call_id=?1",
                    [call_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((state, attempts, next)) = notice else {
                return Ok(false);
            };
            if state != "pending" || attempts >= 3 || now_ms < next {
                return Ok(false);
            }
            tx.execute(
                "UPDATE phone_call_end_notices SET attempts=attempts+1,next_ms=?2 WHERE call_id=?1",
                params![call_id, now_ms.saturating_add(END_NOTICE_LEASE_MS << attempts)],
            )?;
            Ok(true)
        })
    }
    /// The notice reached the durable outbox. False if it was already done.
    pub fn phone_complete_end_notice(&self, call_id: &str, now_ms: i64) -> PhoneResult<bool> {
        transaction(self, |tx| {
            Ok(tx.execute(
                "UPDATE phone_call_end_notices SET state='done',done_ms=?2 WHERE call_id=?1 AND state='pending'",
                params![call_id, now_ms],
            )? == 1)
        })
    }
}

/// Intentionally lacks Debug/Serialize: descriptions and results are private.
pub struct PhoneDispatchJob {
    pub call_id: String,
    pub task_id: String,
    pub description: String,
    pub state: String,
    pub result: Option<String>,
}
impl Store {
    /// Commit intent before telling the caller their work was accepted.
    pub fn phone_enqueue_dispatch(
        &self,
        call_id: &str,
        task_id: &str,
        description: &str,
    ) -> PhoneResult<()> {
        identifier(task_id)?;
        if description.trim().is_empty() || description.len() > 12_000 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT description FROM phone_dispatch_jobs WHERE call_id=?1 AND task_id=?2",
                    params![call_id, task_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                return if existing == description {
                    Ok(())
                } else {
                    Err(PhoneError::Conflict)
                };
            }
            let pending: i64 = tx.query_row(
                "SELECT COUNT(*) FROM phone_dispatch_jobs WHERE state != 'done'",
                [],
                |r| r.get(0),
            )?;
            if pending >= 128 {
                return Err(PhoneError::RateLimited);
            }
            tx.execute("INSERT INTO phone_dispatch_jobs(call_id,task_id,description,state) VALUES(?1,?2,?3,'queued')",
                params![call_id, task_id, description])?;
            Ok(())
        })
    }
    pub fn phone_pending_dispatches(&self) -> PhoneResult<Vec<PhoneDispatchJob>> {
        Ok(self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT call_id,task_id,description,state,result FROM phone_dispatch_jobs WHERE state != 'done' ORDER BY rowid LIMIT 128")?;
            let rows = stmt.query_map([], |r| Ok(PhoneDispatchJob {
                call_id: r.get(0)?, task_id: r.get(1)?, description: r.get(2)?, state: r.get(3)?, result: r.get(4)?,
            }))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }
    /// Claim only immediately before invoking the native agent, after its queue wait.
    pub fn phone_start_dispatch(&self, call_id: &str, task_id: &str) -> PhoneResult<bool> {
        Ok(self.with_conn(|conn| conn.execute("UPDATE phone_dispatch_jobs SET state='running' WHERE call_id=?1 AND task_id=?2 AND state='queued'",
            params![call_id, task_id]))? == 1)
    }
    /// Persist the answer before any attempt to enqueue its chat delivery.
    pub fn phone_finish_dispatch(
        &self,
        call_id: &str,
        task_id: &str,
        result: &str,
    ) -> PhoneResult<()> {
        transaction(self, |tx| {
            let changed = tx.execute("UPDATE phone_dispatch_jobs SET state='ready',result=?3 WHERE call_id=?1 AND task_id=?2 AND state IN ('queued','running')",
                params![call_id, task_id, result])?;
            if changed == 1 {
                tx.execute("UPDATE phone_live_dispatches SET result=?3 WHERE call_id=?1 AND task_id=?2 AND delivered=0 AND EXISTS(SELECT 1 FROM phone_live_sessions s WHERE s.call_id=?1 AND s.active=1)",
                    params![call_id,task_id,result])?;
            }
            Ok(())
        })
    }
    pub fn phone_dispatch_delivered(&self, call_id: &str, task_id: &str) -> PhoneResult<()> {
        self.with_conn(|conn| conn.execute("UPDATE phone_dispatch_jobs SET state='done',result=NULL WHERE call_id=?1 AND task_id=?2 AND state='ready'",
            params![call_id, task_id]))?;
        Ok(())
    }
}

/// Private delivery content; deliberately no Debug or Serialize.
pub struct PhoneLiveResult {
    pub delegation_id: String,
    pub result: String,
}
fn live_binding(tx: &Transaction<'_>, call_id: &str, session_id: &str) -> PhoneResult<bool> {
    let row: Option<(String, bool)> = tx
        .query_row(
            "SELECT session_id,active FROM phone_live_sessions WHERE call_id=?1",
            [call_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((id, active)) if id == session_id => Ok(active),
        _ => Err(PhoneError::Conflict),
    }
}
impl Store {
    /// Trusted ingress only, after callback verification and durable relay claim.
    pub fn phone_bind_live_session(&self, call_id: &str, session_id: &str) -> PhoneResult<()> {
        identifier(session_id)?;
        if session_id.len() > 256 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let call = call_by_id(tx, call_id)?;
            if call.state != CallState::Connected || call.stop_requested || call.verification {
                return Err(PhoneError::Conflict);
            }
            let claimed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_relay_claims WHERE call_id=?1)",
                [call_id],
                |r| r.get(0),
            )?;
            if !claimed {
                return Err(PhoneError::Conflict);
            }
            tx.execute(
                "INSERT OR IGNORE INTO phone_live_sessions(call_id,session_id) VALUES(?1,?2)",
                params![call_id, session_id],
            )?;
            if !live_binding(tx, call_id, session_id)? {
                return Err(PhoneError::Conflict);
            }
            Ok(())
        })
    }
    /// Atomically bind provider identity to one native intent. No provider text
    /// can supply a native task ID or replace an already persisted description.
    pub fn phone_enqueue_live_dispatch(
        &self,
        call_id: &str,
        session_id: &str,
        delegation_id: &str,
        description: &str,
    ) -> PhoneResult<String> {
        identifier(delegation_id)?;
        if delegation_id.len() > 256 || description.trim().is_empty() || description.len() > 12_000
        {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            if !live_binding(tx, call_id, session_id)? {
                return Err(PhoneError::Conflict);
            }
            let call = call_by_id(tx, call_id)?;
            if call.state != CallState::Connected || call.stop_requested || call.verification {
                return Err(PhoneError::Conflict);
            }
            let existing: Option<(String,String)> = tx.query_row(
                "SELECT d.task_id,j.description FROM phone_live_dispatches d JOIN phone_dispatch_jobs j ON j.call_id=d.call_id AND j.task_id=d.task_id WHERE d.call_id=?1 AND d.delegation_id=?2",
                params![call_id,delegation_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((id, old)) = existing {
                return if old == description {
                    Ok(id)
                } else {
                    Err(PhoneError::Conflict)
                };
            }
            let pending: i64 = tx.query_row(
                "SELECT COUNT(*) FROM phone_dispatch_jobs WHERE state != 'done'",
                [],
                |r| r.get(0),
            )?;
            let per_call: i64 = tx.query_row(
                "SELECT COUNT(*) FROM phone_live_dispatches WHERE call_id=?1",
                [call_id],
                |r| r.get(0),
            )?;
            if pending >= 128 || per_call >= 128 {
                return Err(PhoneError::RateLimited);
            }
            let task_id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO phone_dispatch_jobs(call_id,task_id,description,state) VALUES(?1,?2,?3,'queued')",params![call_id,task_id,description])?;
            tx.execute(
                "INSERT INTO phone_live_dispatches(call_id,delegation_id,task_id) VALUES(?1,?2,?3)",
                params![call_id, delegation_id, task_id],
            )?;
            Ok(task_id)
        })
    }
    pub fn phone_live_results(
        &self,
        call_id: &str,
        session_id: &str,
    ) -> PhoneResult<Vec<PhoneLiveResult>> {
        transaction(self, |tx| {
            if !live_binding(tx, call_id, session_id)? {
                return Ok(Vec::new());
            }
            let mut statement = tx.prepare("SELECT delegation_id,CASE WHEN length(CAST(result AS BLOB))>64000 THEN 'The task returned more detail than this voice session can carry. The full result is in our chat; ask for a specific part to discuss.' ELSE result END FROM phone_live_dispatches WHERE call_id=?1 AND delivered=0 AND result IS NOT NULL ORDER BY rowid LIMIT 128")?;
            let rows = statement.query_map([call_id], |r| {
                let mut result: String = r.get(1)?;
                if result.trim().is_empty() { result = "The task returned no readable result.".into(); }
                Ok(PhoneLiveResult {
                    delegation_id: r.get(0)?,
                    result,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
    /// Mark commentary accepted by the Live provider. This is not evidence
    /// that audio played or that the caller heard the result.
    pub fn phone_live_result_delivered(
        &self,
        call_id: &str,
        session_id: &str,
        delegation_id: &str,
    ) -> PhoneResult<()> {
        transaction(self, |tx| {
            live_binding(tx, call_id, session_id)?;
            tx.execute("UPDATE phone_live_dispatches SET delivered=1,result=NULL WHERE call_id=?1 AND delegation_id=?2 AND result IS NOT NULL",params![call_id,delegation_id])?;
            Ok(())
        })
    }
    pub fn phone_end_live_session(&self, call_id: &str, session_id: &str) -> PhoneResult<()> {
        transaction(self, |tx| {
            live_binding(tx, call_id, session_id)?;
            tx.execute(
                "UPDATE phone_live_sessions SET active=0 WHERE call_id=?1",
                [call_id],
            )?;
            tx.execute(
                "UPDATE phone_live_dispatches SET delivered=1,result=NULL WHERE call_id=?1",
                [call_id],
            )?;
            Ok(())
        })
    }
}

impl Store {
    /// Recover receipt cleanup after process death or a failed coordinator Drop.
    /// This never cancels native jobs or changes their original chat outbox.
    pub fn phone_reconcile_live_sessions(&self) -> PhoneResult<()> {
        transaction(self, |tx| {
            tx.execute("UPDATE phone_live_sessions SET active=0 WHERE active=1 AND call_id IN (SELECT id FROM phone_calls WHERE state!='connected' OR stop_requested=1 OR ended_ms IS NOT NULL)", [])?;
            tx.execute("UPDATE phone_live_dispatches SET delivered=1,result=NULL WHERE delivered=0 AND call_id IN (SELECT call_id FROM phone_live_sessions WHERE active=0)", [])?;
            Ok(())
        })
    }
}

impl Store {
    pub fn phone_voice_backend(&self, call_id: &str) -> PhoneResult<String> {
        transaction(self, |tx| {
            call_by_id(tx, call_id)?;
            let backend: Option<String> = tx
                .query_row(
                    "SELECT backend FROM phone_voice_backends WHERE call_id=?1",
                    [call_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(backend.unwrap_or_else(|| "conversation_relay".into()))
        })
    }
    /// Operator configuration, persisted before the provider can receive a dial.
    pub fn phone_set_voice_backend(&self, call_id: &str, backend: &str) -> PhoneResult<()> {
        if !matches!(backend, "conversation_relay" | "openai_live") {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let call = call_by_id(tx, call_id)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT backend FROM phone_voice_backends WHERE call_id=?1",
                    [call_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                return if existing == backend {
                    Ok(())
                } else {
                    Err(PhoneError::Conflict)
                };
            }
            if call.state != CallState::Requesting
                || call.provider_sid.is_some()
                || call.stop_requested
                || call.verification
            {
                return Err(PhoneError::Conflict);
            }
            tx.execute(
                "INSERT INTO phone_voice_backends(call_id,backend) VALUES(?1,?2)",
                params![call_id, backend],
            )?;
            Ok(())
        })
    }
}

#[path = "phone_live_history.rs"]
mod live_history;
pub use live_history::{LiveHistoryBatch, LiveTranscript};

#[path = "phone_live_usage.rs"]
mod live_usage;
pub use live_usage::{LiveUsageFinal, LiveUsagePhase, PhoneLiveUsage};

#[path = "phone_live_summary.rs"]
mod live_summary;
pub use live_summary::PhoneLiveSummaryJob;

#[path = "phone_live_playback.rs"]
mod live_playback;
pub use live_playback::PhoneLivePlayback;
