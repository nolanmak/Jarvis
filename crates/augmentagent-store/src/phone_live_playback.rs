use super::*;
/// Content-free counters. A mark establishes buffer drain, not speech onset.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneLivePlayback {
    pub input_bytes: u64,
    pub output_received_bytes: u64,
    pub output_sent_bytes: u64,
    pub played_bytes: u64,
    pub clear_requested_bytes: u64,
    pub cleared_receipt_bytes: u64,
    pub pending_bytes: u64,
    pub peak_pending_bytes: u64,
    pub first_output_received_ms: Option<u64>,
    pub first_output_sent_ms: Option<u64>,
    pub first_playback_receipt_ms: Option<u64>,
}
impl PhoneLivePlayback {
    fn counters(&self) -> [u64; 7] {
        [
            self.input_bytes,
            self.output_received_bytes,
            self.output_sent_bytes,
            self.played_bytes,
            self.clear_requested_bytes,
            self.cleared_receipt_bytes,
            self.peak_pending_bytes,
        ]
    }
    fn validate(&self) -> PhoneResult<()> {
        if self.counters().iter().any(|v| *v > i64::MAX as u64)
            || self.output_sent_bytes > self.output_received_bytes
            || self
                .played_bytes
                .checked_add(self.cleared_receipt_bytes)
                .is_none_or(|sum| sum > self.output_sent_bytes)
            || self.clear_requested_bytes > self.output_sent_bytes
            || self.pending_bytes > self.peak_pending_bytes
            || self.peak_pending_bytes > 16_000
            || self.peak_pending_bytes > self.output_sent_bytes
            || self.first_output_received_ms.is_some() != (self.output_received_bytes > 0)
            || self.first_output_sent_ms.is_some() != (self.output_sent_bytes > 0)
            || self.first_playback_receipt_ms.is_some() != (self.played_bytes > 0)
            || self
                .first_output_sent_ms
                .zip(self.first_output_received_ms)
                .is_some_and(|(sent, received)| sent < received)
            || self
                .first_playback_receipt_ms
                .zip(self.first_output_sent_ms)
                .is_some_and(|(played, sent)| played < sent)
        {
            return Err(PhoneError::InvalidInput);
        }
        Ok(())
    }
}
impl Store {
    pub fn phone_save_live_playback(
        &self,
        call_id: &str,
        evidence: &PhoneLivePlayback,
        collector_closed: bool,
    ) -> PhoneResult<()> {
        evidence.validate()?;
        let json = serde_json::to_string(evidence).map_err(|_| PhoneError::InvalidInput)?;
        transaction(self, |tx| {
            call_by_id(tx, call_id)?;
            let old: Option<(String, bool)> = tx
                .query_row(
                    "SELECT evidence,collector_closed FROM phone_live_playback WHERE call_id=?1",
                    [call_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((old, closed)) = old {
                let old: PhoneLivePlayback =
                    serde_json::from_str(&old).map_err(|_| PhoneError::InvalidInput)?;
                if closed {
                    return if collector_closed && old == *evidence {
                        Ok(())
                    } else {
                        Err(PhoneError::Conflict)
                    };
                }
                if old
                    .counters()
                    .iter()
                    .zip(evidence.counters())
                    .any(|(old, new)| *old > new)
                    || [
                        old.first_output_received_ms,
                        old.first_output_sent_ms,
                        old.first_playback_receipt_ms,
                    ]
                    .iter()
                    .zip([
                        evidence.first_output_received_ms,
                        evidence.first_output_sent_ms,
                        evidence.first_playback_receipt_ms,
                    ])
                    .any(|(old, new)| old.is_some() && *old != new)
                {
                    return Err(PhoneError::Conflict);
                }
            }
            tx.execute("INSERT INTO phone_live_playback(call_id,evidence,collector_closed) VALUES(?1,?2,?3) ON CONFLICT(call_id) DO UPDATE SET evidence=excluded.evidence,collector_closed=excluded.collector_closed",params![call_id,json,collector_closed])?;
            Ok(())
        })
    }
    pub fn phone_live_playback(
        &self,
        last_calls: usize,
    ) -> PhoneResult<Vec<(PhoneLivePlayback, bool)>> {
        if !(1..=1000).contains(&last_calls) {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            let mut statement=tx.prepare("SELECT evidence,collector_closed FROM phone_live_playback WHERE call_id IN (SELECT id FROM phone_calls ORDER BY created_ms DESC,id DESC LIMIT ?1)")?;
            let rows = statement
                .query_map([last_calls as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .map(|(json, closed)| {
                    Ok((
                        serde_json::from_str(&json).map_err(|_| PhoneError::InvalidInput)?,
                        closed,
                    ))
                })
                .collect()
        })
    }
}
