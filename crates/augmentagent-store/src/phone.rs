//! Durable owner-phone state. Provider I/O happens only after a transaction
//! grants a single dial permit; a process death never recreates that permit.
//! Destinations are deliberately absent from serializable call receipts.
use crate::{Store, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use thiserror::Error;

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(include_str!("phone.sql"))?;
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
    let day_start = request.now_ms / 86_400_000 * 86_400_000;
    let used_ms: i64 = tx.query_row(
                "SELECT COALESCE(SUM(MAX(0,MIN(COALESCE(ended_ms,?2),?2)-MAX(answered_ms,?3))),0) FROM phone_calls WHERE owner=?1 AND answered_ms IS NOT NULL",
                params![request.owner,request.now_ms,day_start], |r|r.get(0))?;
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

impl Store {
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
            if call.provider_sid.as_deref().is_some_and(|old| old != sid) {
                return Err(PhoneError::Conflict);
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
