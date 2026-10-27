//! Signed provider callbacks. HTTP adapters must pass every decoded form field,
//! with no trimming, and reject non-form content types and oversized requests.
use crate::{
    twilio::validate_signature,
    voice::{hints, TtsProvider},
    xml, Error, PhoneService,
};
use augmentagent_store::phone::{CallState, PhoneCall, PhoneError, ProviderUsage};

pub struct WebhookAuth {
    account: String,
    token: String,
}
impl WebhookAuth {
    pub(crate) fn account(&self) -> &str {
        &self.account
    }
    pub(crate) fn verify_socket(&self, url: &str, signature: &str) -> bool {
        !self.token.is_empty() && validate_signature(&self.token, url, &[], signature)
    }

    pub fn new(account: &str, token: &str) -> Self {
        Self {
            account: account.into(),
            token: token.into(),
        }
    }
    pub(crate) fn verify(
        &self,
        url: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<(), Error> {
        if self.token.is_empty() || !validate_signature(&self.token, url, fields, signature) {
            return Err(Error::Unauthorized);
        }
        if field(fields, "AccountSid")? != self.account {
            return Err(Error::Unauthorized);
        }
        Ok(())
    }
}
fn field<'a>(fields: &'a [(String, String)], name: &str) -> Result<&'a str, Error> {
    let mut matches = fields.iter().filter(|(k, _)| k == name);
    let (_, value) = matches.next().ok_or(Error::InvalidArguments)?;
    if matches.next().is_some() {
        return Err(Error::InvalidArguments);
    }
    Ok(value)
}
const HANGUP: &str = "<Response><Hangup/></Response>";
impl PhoneService {
    pub fn relay_ended_callback(
        &self,
        auth: &WebhookAuth,
        id: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<String, Error> {
        self.callback_call(auth, "relay-ended", id, fields, signature)?;
        // #1557: first, so a summary-queue failure cannot drop the reason.
        self.record_end_reason(id, fields);
        // #1552: the conversation is over; queue its summary exactly once.
        self.store.phone_enqueue_call_summary(id, self.clock.now_ms())?;
        let failed = field(fields, "SessionStatus")? == "failed";
        if failed {
            // Never log provider error text: it can include URLs and caller data.
            let code = fields
                .iter()
                .find(|(key, _)| key == "ErrorCode")
                .and_then(|(_, value)| value.parse::<u32>().ok());
            tracing::warn!(call_id = id, error_code = ?code, "phone relay provider connection failed");
            return Ok("<Response><Say voice=\"Polly.Joanna\" language=\"en-US\">Sorry, Jarvis could not connect the voice conversation. The connection error has been recorded.</Say><Hangup/></Response>".into());
        }
        Ok(HANGUP.into())
    }
    /// #1557: an agent-initiated `end` carries `handoffData`; record why the
    /// call ended. Absent means the agent did not end it. Malformed data is
    /// categorized, never logged.
    fn record_end_reason(&self, id: &str, fields: &[(String, String)]) {
        let mut handoff = fields.iter().filter(|(k, _)| k == "HandoffData");
        let Some((_, data)) = handoff.next() else {
            return;
        };
        let reason = if handoff.next().is_some() {
            crate::end_call::UNKNOWN_END_REASON
        } else {
            crate::end_call::end_reason_from_handoff(data, id)
        };
        if reason == crate::end_call::UNKNOWN_END_REASON {
            tracing::warn!(call_id = id, category = "handoff_data_invalid", "phone relay-ended handoff data not recognized");
        }
        match self
            .store
            .phone_record_end_reason(id, reason, self.clock.now_ms())
        {
            Ok(true) => {
                tracing::info!(call_id = id, end_reason = reason, "phone call end reason recorded")
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(call_id = id, error = %error, "phone call end reason not persisted")
            }
        }
    }
    fn callback_call(
        &self,
        auth: &WebhookAuth,
        kind: &str,
        id: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<(PhoneCall, String), Error> {
        if uuid::Uuid::parse_str(id).is_err()
            || fields.len() > 100
            || fields.iter().any(|(k, v)| k.len() > 256 || v.len() > 8192)
        {
            return Err(Error::InvalidArguments);
        }
        auth.verify(
            &format!("{}/phone/{kind}/{id}", self.config.public_base),
            fields,
            signature,
        )?;
        let sid = field(fields, "CallSid")?;
        let call = self.store.phone_provider_call(id)?;
        if call.provider_sid.as_deref().is_some_and(|s| s != sid) {
            return Err(Error::Unauthorized);
        }
        Ok((call, sid.to_owned()))
    }
    pub fn status_callback(
        &self,
        auth: &WebhookAuth,
        id: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<(), Error> {
        let (_, sid) = self.callback_call(auth, "status", id, fields, signature)?;
        let state = match field(fields, "CallStatus")? {
            "queued" | "initiated" => CallState::Queued,
            "ringing" => CallState::Ringing,
            "in-progress" => CallState::AwaitingAcceptance,
            "completed" => CallState::Completed,
            "busy" => CallState::Busy,
            "no-answer" => CallState::NoAnswer,
            "canceled" => CallState::Canceled,
            "failed" => CallState::Failed,
            _ => return Err(Error::InvalidArguments),
        };
        let usage = if fields.iter().any(|(name, _)| name == "CallDuration") {
            let seconds = field(fields, "CallDuration")?;
            if !state.terminal()
                || seconds.is_empty()
                || !seconds.bytes().all(|c| c.is_ascii_digit())
            {
                return Err(Error::InvalidArguments);
            }
            let duration_seconds = seconds
                .parse::<i64>()
                .map_err(|_| Error::InvalidArguments)?;
            // Callback receipt time is not the call end time: delivery can be
            // late or cross midnight. Configured progress callbacks carry the
            // signed RFC 2822 event Timestamp alongside CallDuration (seconds).
            let ended_ms = chrono::DateTime::parse_from_rfc2822(field(fields, "Timestamp")?)
                .map_err(|_| Error::InvalidArguments)?
                .timestamp_millis();
            Some(ProviderUsage {
                duration_seconds,
                ended_ms,
            })
        } else {
            None
        };
        self.store
            .phone_provider_event_with_usage(id, &sid, state, self.clock.now_ms(), usage)?;
        if state.terminal() {
            // #1552: also covers a relay-ended callback that never arrived.
            // Calls that never reached the relay are not enqueued.
            self.store.phone_enqueue_call_summary(id, self.clock.now_ms())?;
        }
        Ok(())
    }
    pub fn start_callback(
        &self,
        auth: &WebhookAuth,
        id: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<String, Error> {
        let (call, sid) = self.callback_call(auth, "start", id, fields, signature)?;
        if call.verification || call.stop_requested || call.state.terminal() || !self.config.enabled
        {
            return Ok(HANGUP.into());
        }
        if self.authorize_call_origin(&call).is_err() {
            self.store.phone_request_stop(&call.owner, id)?;
            return Ok(HANGUP.into());
        }
        self.store.phone_provider_event(
            id,
            &sid,
            CallState::AwaitingAcceptance,
            self.clock.now_ms(),
        )?;
        let current = self.store.phone_provider_call(id)?;
        // Replayed start callbacks must not restart the acceptance window.
        if current.state == CallState::Connected
            || current
                .answered_ms
                .is_some_and(|t| self.clock.now_ms() - t >= 15_000)
        {
            return Ok(HANGUP.into());
        }
        // The authenticated chat request already authorizes this owner call.
        // The signed answer callback connects it; no second DTMF consent step.
        match self.store.phone_accept(id, &sid, self.clock.now_ms()) {
            Ok(_) => self.connection_twiml(id),
            Err(PhoneError::AcceptanceExpired | PhoneError::Conflict) => Ok(HANGUP.into()),
            Err(error) => Err(error.into()),
        }
    }
    pub async fn accept_callback(
        &self,
        auth: &WebhookAuth,
        id: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<String, Error> {
        let (call, sid) = self.callback_call(auth, "accept", id, fields, signature)?;
        if call.verification || call.stop_requested || call.state.terminal() || !self.config.enabled
        {
            return Ok(HANGUP.into());
        }
        if self.authorize_call_origin(&call).is_err() {
            self.store.phone_request_stop(&call.owner, id)?;
            return Ok(HANGUP.into());
        }
        if field(fields, "Digits")? != "1" {
            self.store.phone_request_stop(&call.owner, id)?;
            return Ok(HANGUP.into());
        }
        match self.store.phone_accept(id, &sid, self.clock.now_ms()) {
            Ok(_) => {}
            Err(PhoneError::AcceptanceExpired | PhoneError::Conflict) => return Ok(HANGUP.into()),
            Err(e) => return Err(e.into()),
        }
        self.connection_twiml(id)
    }
    fn connection_twiml(&self, id: &str) -> Result<String, Error> {
        if self.store.phone_voice_backend(id)? == "openai_live" {
            if self.config.live.is_none() { return Err(Error::NotConfigured); }
            let ws = self.config.public_base.replacen("https://", "wss://", 1);
            return Ok(format!("<Response><Connect><Stream url=\"{}/phone/relay/{id}\" /></Connect><Hangup/></Response>", xml(&ws)));
        }
        Ok(self.relay_twiml(id))
    }
    fn relay_twiml(&self, id: &str) -> String {
        // Every attribute value here is startup configuration validated to an
        // attribute-safe alphabet, or a constant. Nothing from the call, the
        // provider callback or the agent is read.
        let ws = self.config.public_base.replacen("https://", "wss://", 1);
        // #1551: tokens-played/speaker events measure what the owner hears.
        let debug = if self.config.debug_events {
            " debug=\"debugging tokens-played speaker-events\""
        } else {
            ""
        };
        // #1555: interim recognition only when the fast layer may speculate.
        let partial = if self.config.preemptive && self.config.fast.is_some() {
            "true"
        } else {
            "false"
        };
        let voice = &self.config.voice;
        let normalization = match voice.tts_provider() {
            TtsProvider::ElevenLabs => " elevenlabsTextNormalization=\"on\"",
            TtsProvider::Amazon | TtsProvider::Google => "",
        };
        format!(
            "<Response><Connect action=\"{}/phone/relay-ended/{id}\" method=\"POST\"><ConversationRelay url=\"{}/phone/relay/{id}\" transcriptionProvider=\"{}\" speechModel=\"{}\" ttsProvider=\"{}\" voice=\"{}\" language=\"en-US\"{normalization} hints=\"{}\" interruptible=\"any\" reportInputDuringAgentSpeech=\"any\" partialPrompts=\"{partial}\" welcomeGreeting=\"Hi, I'm here. What can I help you with?\"{debug} /></Connect><Hangup/></Response>",
            xml(&self.config.public_base),
            xml(&ws),
            voice.stt_provider().as_str(),
            xml(voice.stt_model()),
            voice.tts_provider().as_str(),
            xml(voice.tts_voice()),
            xml(&hints(self.config.owner_first_name.as_deref())),
        )
    }
}
