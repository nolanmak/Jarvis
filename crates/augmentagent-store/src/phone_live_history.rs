//! Private, bounded transcript delivery; no provider or chat I/O in transactions.
use super::*;
use sha2::{Digest, Sha256};

// Deliberately no Debug/Serialize: these carry private conversation text.
pub struct LiveTranscript {
    pub event_id: String,
    pub user: bool,
    pub text: String,
    pub start_ms: f64,
    pub end_ms: f64,
}
pub struct LiveHistoryBatch {
    pub call_id: String,
    pub key: String,
    pub text: String,
}
impl Store {
    pub fn phone_append_live_transcript(
        &self,
        call_id: &str,
        session_id: &str,
        fragment: &LiveTranscript,
    ) -> PhoneResult<bool> {
        identifier(&fragment.event_id)?;
        if fragment.event_id.len() > 256
            || fragment.text.len() > 8000
            || !fragment.start_ms.is_finite()
            || !fragment.end_ms.is_finite()
            || fragment.start_ms < 0.
            || fragment.end_ms < fragment.start_ms
            || fragment.end_ms > 86_400_000.
        {
            return Err(PhoneError::InvalidInput);
        }
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    fragment.user,
                    &fragment.text,
                    fragment.start_ms,
                    fragment.end_ms,
                ))
                .map_err(|_| PhoneError::InvalidInput)?
            )
        );
        transaction(self, |tx| {
            if !live_binding(tx, call_id, session_id)? {
                return Err(PhoneError::Conflict);
            }
            let call = call_by_id(tx, call_id)?;
            if call.state != CallState::Connected || call.stop_requested || call.verification {
                return Err(PhoneError::Conflict);
            }
            let old: Option<String> = tx.query_row("SELECT fingerprint FROM phone_live_transcripts WHERE call_id=?1 AND event_id=?2", params![call_id,fragment.event_id], |r| r.get(0)).optional()?;
            if let Some(old) = old {
                return if old == fingerprint {
                    Ok(Some(false))
                } else {
                    Err(PhoneError::Conflict)
                };
            }
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM phone_live_transcripts WHERE call_id=?1 AND text IS NOT NULL",
                [call_id],
                |r| r.get(0),
            )?;
            let total: i64 = tx.query_row("SELECT COUNT(*) FROM phone_live_transcripts WHERE call_id=?1", [call_id], |r| r.get(0))?;
            let bytes: i64 = tx.query_row(
                "SELECT COALESCE(SUM(length(CAST(text AS BLOB))),0) FROM phone_live_transcripts WHERE text IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            if count >= 8192 || total >= 65_536 || bytes + fragment.text.len() as i64 > 1_048_576 {
                tx.execute("INSERT OR IGNORE INTO phone_live_history_gaps(call_id) VALUES(?1)", [call_id])?;
                return Ok(None);
            }
            tx.execute("INSERT INTO phone_live_transcripts(call_id,event_id,fingerprint,user,text) VALUES(?1,?2,?3,?4,?5)", params![call_id,fragment.event_id,fingerprint,fragment.user,fragment.text])?;
            Ok(Some(true))
        })?.ok_or(PhoneError::RateLimited)
    }
    /// Freeze one batch per call. Later fragments cannot change a retry's key
    /// or payload. The adapter must accept the key into its durable outbox.
    pub fn phone_live_history_batches(&self) -> PhoneResult<Vec<LiveHistoryBatch>> {
        transaction(self, |tx| {
            let mut statement = tx.prepare("SELECT t.call_id FROM phone_live_transcripts t WHERE t.text IS NOT NULL GROUP BY t.call_id ORDER BY COALESCE((SELECT b.attempt_order FROM phone_live_history_batches b WHERE b.call_id=t.call_id AND b.delivered=0 LIMIT 1),0),MIN(t.rowid) LIMIT 32")?;
            let calls = statement
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut batches = Vec::new();
            for call_id in calls {
                let existing = tx.query_row("SELECT id,text FROM phone_live_history_batches WHERE call_id=?1 AND delivered=0 ORDER BY rowid LIMIT 1", [&call_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
                let (key, text) = if let Some(existing) = existing {
                    existing
                } else {
                    let mut statement = tx.prepare("SELECT event_id,user,text FROM phone_live_transcripts WHERE call_id=?1 AND batch_id IS NULL AND text IS NOT NULL ORDER BY rowid LIMIT 64")?;
                    let fragments = statement
                        .query_map([&call_id], |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, bool>(1)?,
                                r.get::<_, String>(2)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    let mut text = String::new();
                    let mut ids = Vec::new();
                    let mut previous_role = None;
                    for (id, user, fragment) in fragments {
                        if !ids.is_empty() && text.len() + fragment.len() + 80 > 12_000 {
                            break;
                        }
                        if previous_role != Some(user) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(if user {
                                "Phone — you: "
                            } else {
                                "Phone — Jarvis (generated; playback unconfirmed): "
                            });
                        }
                        text.push_str(&fragment);
                        previous_role = Some(user);
                        ids.push(id);
                    }
                    if ids.is_empty() {
                        continue;
                    }
                    let key = format!("phone:{call_id}:live-history:{}", uuid::Uuid::new_v4());
                    tx.execute(
                        "INSERT INTO phone_live_history_batches(id,call_id,text) VALUES(?1,?2,?3)",
                        params![key, call_id, text],
                    )?;
                    for id in ids {
                        tx.execute("UPDATE phone_live_transcripts SET batch_id=?1 WHERE call_id=?2 AND event_id=?3", params![key,call_id,id])?;
                    }
                    (key, text)
                };
                // Rotate failed/revoked deliveries behind untouched calls so
                // an old backlog cannot monopolize the bounded polling page.
                tx.execute("UPDATE phone_live_history_batches SET attempt_order=(SELECT COALESCE(MAX(attempt_order),0)+1 FROM phone_live_history_batches) WHERE id=?1", [&key])?;
                batches.push(LiveHistoryBatch { call_id, key, text });
            }
            Ok(batches)
        })
    }
    pub fn phone_live_history_delivered(&self, call_id: &str, key: &str) -> PhoneResult<()> {
        transaction(self, |tx| {
            let changed = tx.execute("UPDATE phone_live_history_batches SET delivered=1,text=NULL WHERE call_id=?1 AND id=?2", params![call_id,key])?;
            if changed != 1 {
                return Err(PhoneError::Conflict);
            }
            tx.execute(
                "UPDATE phone_live_transcripts SET text=NULL WHERE call_id=?1 AND batch_id=?2",
                params![call_id, key],
            )?;
            Ok(())
        })
    }
    pub fn phone_live_history_pending(&self, call_id: &str) -> PhoneResult<bool> {
        transaction(self, |tx| {
            Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_live_transcripts WHERE call_id=?1 AND text IS NOT NULL)", [call_id], |r| r.get(0))?)
        })
    }
}

const HISTORY_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const HISTORY_GAP_NOTICE: &str = "Part of this phone conversation could not be retained in chat. The transcript is incomplete, so automatic memory extraction is disabled for this call. Task execution and task results are unaffected.";
impl Store {
    pub fn phone_record_live_history_gap(&self, call_id: &str) -> PhoneResult<()> {
        transaction(self, |tx| {
            call_by_id(tx, call_id)?;
            tx.execute("INSERT OR IGNORE INTO phone_live_history_gaps(call_id) VALUES(?1)", [call_id])?;
            Ok(())
        })
    }
    pub fn phone_live_history_incomplete(&self, call_id: &str) -> PhoneResult<bool> {
        transaction(self, |tx| Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_live_history_gaps WHERE call_id=?1)", [call_id], |r| r.get(0))?))
    }
    /// Expire only undelivered staging text from ended calls. Keep event
    /// fingerprints for replay protection; never mark expired batches delivered.
    /// At most 32 calls per pass, with no provider or chat I/O under the lock.
    pub fn phone_expire_live_history(&self, now_ms: i64) -> PhoneResult<usize> {
        transaction(self, |tx| {
            let mut stmt = tx.prepare("SELECT id FROM phone_calls WHERE ended_ms IS NOT NULL AND ended_ms<=?1 AND EXISTS(SELECT 1 FROM phone_live_transcripts t WHERE t.call_id=phone_calls.id AND t.text IS NOT NULL) ORDER BY ended_ms,id LIMIT 32")?;
            let ids = stmt.query_map([now_ms.saturating_sub(HISTORY_RETENTION_MS)], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for id in &ids {
                tx.execute("INSERT OR IGNORE INTO phone_live_history_gaps(call_id) VALUES(?1)", [id])?;
                tx.execute("UPDATE phone_live_transcripts SET text=NULL,batch_id=NULL WHERE call_id=?1 AND text IS NOT NULL", [id])?;
                tx.execute("DELETE FROM phone_live_history_batches WHERE call_id=?1 AND delivered=0", [id])?;
            }
            Ok(ids.len())
        })
    }
    pub fn phone_live_history_notices(&self) -> PhoneResult<Vec<LiveHistoryBatch>> {
        transaction(self, |tx| {
            let mut stmt = tx.prepare("SELECT call_id FROM phone_live_history_gaps WHERE notice_delivered=0 ORDER BY attempt_order,call_id LIMIT 32")?;
            let ids = stmt.query_map([], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut result = Vec::new();
            for call_id in ids {
                tx.execute("UPDATE phone_live_history_gaps SET attempt_order=(SELECT COALESCE(MAX(attempt_order),0)+1 FROM phone_live_history_gaps) WHERE call_id=?1", [&call_id])?;
                result.push(LiveHistoryBatch { key:format!("phone:{call_id}:live-history-incomplete"), call_id, text:HISTORY_GAP_NOTICE.into() });
            }
            Ok(result)
        })
    }
    pub fn phone_live_history_notice_delivered(&self, call_id: &str) -> PhoneResult<()> {
        transaction(self, |tx| {
            if tx.execute("UPDATE phone_live_history_gaps SET notice_delivered=1 WHERE call_id=?1", [call_id])? != 1 { return Err(PhoneError::Conflict); }
            Ok(())
        })
    }
}
