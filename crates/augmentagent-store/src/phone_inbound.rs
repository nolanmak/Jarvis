//! Signed inbound provider ingress, sharing outbound concurrency and budgets.
use super::*;

pub struct ReserveInboundCall<'a> {
    pub owner: &'a str,
    pub caller: &'a str,
    pub provider_sid: &'a str,
    pub origin: &'a PhoneOrigin,
    pub revision: i64,
    pub backend: &'a str,
    pub now_ms: i64,
}

impl Store {
    /// Number matching is only routing; the service must also require the PIN.
    pub fn phone_owners_for_number(&self, number: &str) -> PhoneResult<Vec<String>> {
        validate_e164(number)?;
        Ok(self.with_conn(|c| {
            c.prepare("SELECT owner FROM phone_destinations WHERE number=?1")?
                .query_map([number], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }

    pub fn phone_is_inbound(&self, id: &str) -> PhoneResult<bool> {
        Ok(self.with_conn(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM phone_inbound_calls WHERE call_id=?1)",
                [id],
                |r| r.get(0),
            )
        })?)
    }

    pub fn phone_inbound_by_sid(&self, sid: &str) -> PhoneResult<PhoneCall> {
        let id: Option<String> = self.with_conn(|c| c.query_row(
            "SELECT c.id FROM phone_calls c JOIN phone_inbound_calls i ON i.call_id=c.id WHERE c.provider_sid=?1",
            [sid], |r| r.get(0),
        ).optional())?;
        self.phone_provider_call(&id.ok_or(PhoneError::NotFound)?)
    }

    /// Reservation, provider binding, backend and original chat commit together.
    /// Retries never create another call or reset the PIN acceptance window.
    pub fn phone_reserve_inbound(&self, input: ReserveInboundCall<'_>) -> PhoneResult<PhoneCall> {
        identifier(input.owner)?;
        identifier(input.provider_sid)?;
        validate_e164(input.caller)?;
        if input.now_ms < 0
            || !matches!(input.backend, "conversation_relay" | "openai_live")
            || input.origin.owner.account() != input.origin.conversation.account()
        {
            return Err(PhoneError::InvalidInput);
        }
        let json = serde_json::to_string(input.origin).map_err(|_| PhoneError::InvalidInput)?;
        let conversation = input.origin.conversation.storage_key();
        transaction(self, |tx| {
            let number: Option<String> = tx
                .query_row(
                    "SELECT number FROM phone_destinations WHERE owner=?1",
                    [input.owner],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if number.as_deref() != Some(input.caller) {
                return Err(PhoneError::Conflict);
            }
            let current: Option<i64> = tx.query_row(
                "SELECT r.revision FROM surface_authority_revisions r JOIN surface_owner_bindings b
                 USING(platform,account_id) WHERE r.platform=?1 AND r.account_id=?2 AND b.owner_sender_id=?3",
                params![input.origin.owner.account().platform().as_str(), input.origin.owner.account().account_id(), input.origin.owner.sender_id()],
                |r| r.get(0),
            ).optional()?;
            if current != Some(input.revision) {
                return Err(PhoneError::Conflict);
            }
            let reserved = reserve(
                tx,
                ReserveCall {
                    owner: input.owner,
                    request_id: &format!("inbound:{}", input.provider_sid),
                    conversation: &conversation,
                    now_ms: input.now_ms,
                },
                Some(input.caller),
                false,
            )?;
            let id = &reserved.call.id;
            if !reserved.should_dial {
                let old: Option<(String, i64)> = tx.query_row(
                    "SELECT o.origin_json,a.revision FROM phone_origins o JOIN phone_origin_authority a USING(call_id)
                     JOIN phone_inbound_calls i USING(call_id) WHERE call_id=?1", [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?;
                if old != Some((json, input.revision))
                    || reserved.call.provider_sid.as_deref() != Some(input.provider_sid)
                {
                    return Err(PhoneError::Conflict);
                }
                return Ok(reserved.call);
            }
            tx.execute("UPDATE phone_calls SET provider_sid=?2,state='awaiting_acceptance',answered_ms=?3 WHERE id=?1",
                params![id,input.provider_sid,input.now_ms])?;
            tx.execute("INSERT INTO phone_inbound_calls(call_id) VALUES(?1)", [id])?;
            tx.execute("INSERT INTO phone_origins VALUES(?1,?2)", params![id, json])?;
            tx.execute(
                "INSERT INTO phone_origin_authority VALUES(?1,?2)",
                params![id, input.revision],
            )?;
            tx.execute(
                "INSERT INTO phone_voice_backends VALUES(?1,?2)",
                params![id, input.backend],
            )?;
            call_by_id(tx, id)
        })
    }

    /// Consume one attempt atomically, including failures and expired challenges.
    pub fn phone_accept_inbound(
        &self,
        id: &str,
        sid: &str,
        pin_matches: bool,
        now_ms: i64,
    ) -> PhoneResult<bool> {
        transaction(self, |tx| {
            let call = call_by_id(tx, id)?;
            if call.provider_sid.as_deref() != Some(sid) {
                return Err(PhoneError::Conflict);
            }
            let claimed = tx.execute("UPDATE phone_inbound_calls SET pin_attempted=1 WHERE call_id=?1 AND pin_attempted=0", [id])?;
            if claimed == 0 || call.state != CallState::AwaitingAcceptance || call.stop_requested {
                return Ok(false);
            }
            let allowed = pin_matches
                && call
                    .answered_ms
                    .is_some_and(|t| now_ms >= t && now_ms - t < 15_000);
            if allowed {
                tx.execute("UPDATE phone_calls SET state='connected' WHERE id=?1", [id])?;
            } else {
                tx.execute("UPDATE phone_calls SET stop_requested=1 WHERE id=?1", [id])?;
            }
            Ok(allowed)
        })
    }
}
