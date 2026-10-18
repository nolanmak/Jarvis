//! Signed provider callbacks. HTTP adapters must pass every decoded form field,
//! with no trimming, and reject non-form content types and oversized requests.
use crate::{twilio::validate_signature, xml, Error, PhoneService};
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
        let action = format!("{}/phone/accept/{id}", self.config.public_base);
        Ok(format!("<Response><Gather input=\"dtmf\" numDigits=\"1\" timeout=\"15\" action=\"{}\" method=\"POST\"><Say>Jarvis is calling. Press one to continue.</Say></Gather><Hangup/></Response>",xml(&action)))
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
        let ws = self.config.public_base.replacen("https://", "wss://", 1);
        Ok(format!("<Response><Connect><ConversationRelay url=\"{}/phone/relay/{id}\" interruptible=\"true\" partialPrompts=\"false\" welcomeGreeting=\"Connected to Jarvis.\" /></Connect><Hangup/></Response>",xml(&ws)))
    }
}
