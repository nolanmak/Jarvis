//! Incoming calls use an explicit chat link, a saved caller number and a PIN.
use crate::{
    webhook::{field, WebhookAuth},
    xml, Error, Grant, PhoneService,
};
use augmentagent_store::phone::{CallState, PhoneCall, PhoneOrigin, ReserveInboundCall};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const HANGUP: &str = "<Response><Hangup/></Response>";

/// Startup secrets only; never serialize or include in diagnostics/history.
pub struct InboundConfig {
    number: String,
    platform: String,
    account: String,
    pin: String,
}
impl InboundConfig {
    pub fn new(number: &str, platform: &str, account: &str, pin: &str) -> Result<Self, Error> {
        augmentagent_store::phone::validate_e164(number).map_err(|_| Error::NotConfigured)?;
        augmentagent_store::SurfacePlatform::new(platform).map_err(|_| Error::NotConfigured)?;
        if account.is_empty() || account.len() > 512 || account.chars().any(char::is_control) {
            return Err(Error::InvalidSetting("AUGMENTAGENT_PHONE_INBOUND_ACCOUNT"));
        }
        if !(4..=12).contains(&pin.len()) || !pin.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidSetting("AUGMENTAGENT_PHONE_INBOUND_PIN"));
        }
        Ok(Self {
            number: number.into(),
            platform: platform.into(),
            account: account.into(),
            pin: pin.into(),
        })
    }
    pub(crate) fn from_lookup(
        get: &impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, Error> {
        let Some(pin) = get("AUGMENTAGENT_PHONE_INBOUND_PIN") else {
            return Ok(None);
        };
        let required = |key| get(key).ok_or(Error::InvalidSetting(key));
        Ok(Some(Self::new(
            &required("TWILIO_PHONE_NUMBER")?,
            &required("AUGMENTAGENT_PHONE_INBOUND_PLATFORM")?,
            &required("AUGMENTAGENT_PHONE_INBOUND_ACCOUNT")?,
            &pin,
        )?))
    }
    pub(crate) fn targets(&self, link: &crate::OwnerLink) -> bool {
        link.surface_owner.account().platform().as_str() == self.platform
            && link.surface_owner.account().account_id() == self.account
    }
    fn matches(&self, candidate: &str, key: &[u8]) -> bool {
        let mut expected = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
        expected.update(self.pin.as_bytes());
        let mut supplied = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
        supplied.update(candidate.as_bytes());
        supplied
            .verify_slice(&expected.finalize().into_bytes())
            .is_ok()
    }
}

impl PhoneService {
    fn inbound_fields<'a>(
        &self,
        auth: &WebhookAuth,
        path: &str,
        fields: &'a [(String, String)],
        signature: &str,
    ) -> Result<(&'a str, &'a str), Error> {
        if fields.len() > 100 || fields.iter().any(|(k, v)| k.len() > 256 || v.len() > 8192) {
            return Err(Error::InvalidArguments);
        }
        auth.verify(
            &format!("{}{path}", self.config.public_base),
            fields,
            signature,
        )?;
        let config = self.config.inbound.as_ref().ok_or(Error::Disabled)?;
        if field(fields, "Direction")? != "inbound" || field(fields, "To")? != config.number {
            return Err(Error::Unauthorized);
        }
        let sid = field(fields, "CallSid")?;
        if sid.len() != 34
            || !sid.starts_with("CA")
            || !sid.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
        {
            return Err(Error::InvalidArguments);
        }
        Ok((sid, field(fields, "From")?))
    }
    fn inbound_grant(&self, caller: &str) -> Result<Grant, Error> {
        let config = self.config.inbound.as_ref().ok_or(Error::Disabled)?;
        let owners = self
            .store
            .phone_owners_for_number(caller)
            .map_err(|_| Error::Unauthorized)?;
        if owners.len() != 1 {
            return Err(Error::Unauthorized);
        }
        let link = self
            .config
            .links
            .iter()
            .find(|link| link.owner_id == owners[0] && config.targets(link))
            .ok_or(Error::Unauthorized)?;
        let binding = self
            .store
            .surface_owner_binding(link.surface_owner.account())
            .map_err(|_| Error::Unauthorized)?
            .ok_or(Error::Unauthorized)?;
        let conversation = binding
            .direct_conversation()
            .or_else(|| binding.control_channel())
            .ok_or(Error::Unauthorized)?;
        self.authorize(&link.surface_owner, conversation)
    }
    pub fn incoming_callback(
        &self,
        auth: &WebhookAuth,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<String, Error> {
        let (sid, caller) = self.inbound_fields(auth, "/phone/incoming", fields, signature)?;
        if self.enabled().is_err() {
            return Ok(HANGUP.into());
        }
        let grant = match self.inbound_grant(caller) {
            Ok(grant) => grant,
            Err(_) => return Ok("<Response><Reject reason=\"rejected\"/></Response>".into()),
        };
        let call = match self.store.phone_reserve_inbound(ReserveInboundCall {
            owner: &grant.owner_id,
            caller,
            provider_sid: sid,
            origin: &PhoneOrigin {
                owner: grant.surface_owner,
                conversation: grant.conversation,
            },
            revision: grant.binding_revision,
            backend: self.voice_backend().as_str(),
            now_ms: self.clock.now_ms(),
        }) {
            Ok(call) => call,
            Err(
                augmentagent_store::phone::PhoneError::AlreadyActive
                | augmentagent_store::phone::PhoneError::RateLimited
                | augmentagent_store::phone::PhoneError::BudgetExhausted,
            ) => return Ok("<Response><Reject reason=\"busy\"/></Response>".into()),
            Err(error) => return Err(error.into()),
        };
        if call.stop_requested
            || call.state != CallState::AwaitingAcceptance
            || call
                .answered_ms
                .is_none_or(|t| self.clock.now_ms() - t >= 15_000)
        {
            return Ok(HANGUP.into());
        }
        let config = self.config.inbound.as_ref().ok_or(Error::Disabled)?;
        Ok(format!("<Response><Gather input=\"dtmf\" numDigits=\"{}\" timeout=\"10\" actionOnEmptyResult=\"true\" action=\"{}/phone/incoming/{}\" method=\"POST\"><Say voice=\"Polly.Joanna\">Enter your Jarvis PIN.</Say></Gather><Hangup/></Response>", config.pin.len(), xml(&self.config.public_base), call.id))
    }
    fn inbound_call(&self, id: &str, sid: &str, caller: &str) -> Result<PhoneCall, Error> {
        let call = self.store.phone_inbound_by_sid(sid)?;
        if call.id != id || self.store.phone_dial_destination(id)?.expose() != caller {
            return Err(Error::Unauthorized);
        }
        Ok(call)
    }
    pub fn incoming_pin_callback(
        &self,
        auth: &WebhookAuth,
        id: &str,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<String, Error> {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(Error::InvalidArguments);
        }
        let (sid, caller) =
            self.inbound_fields(auth, &format!("/phone/incoming/{id}"), fields, signature)?;
        let call = self.inbound_call(id, sid, caller)?;
        if self.enabled().is_err()
            || self.authorize_call_origin(&call).is_err()
            || self.inbound_grant(caller).is_err()
        {
            self.store.phone_request_stop(&call.owner, id)?;
            return Ok(HANGUP.into());
        }
        let pin = if fields.iter().any(|(name, _)| name == "Digits") {
            field(fields, "Digits")?
        } else {
            // Gather's actionOnEmptyResult can omit Digits entirely.
            ""
        };
        let matches = self
            .config
            .inbound
            .as_ref()
            .ok_or(Error::Disabled)?
            .matches(pin, &self.config.verification_key);
        if self
            .store
            .phone_accept_inbound(id, sid, matches, self.clock.now_ms())?
        {
            self.connection_twiml(id)
        } else {
            Ok(HANGUP.into())
        }
    }
    pub fn incoming_status_callback(
        &self,
        auth: &WebhookAuth,
        fields: &[(String, String)],
        signature: &str,
    ) -> Result<(), Error> {
        let (sid, caller) =
            self.inbound_fields(auth, "/phone/incoming/status", fields, signature)?;
        let call = match self.store.phone_inbound_by_sid(sid) {
            Ok(call) => call,
            // Rejected unknown/busy callers never acquire local call state.
            Err(augmentagent_store::phone::PhoneError::NotFound) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        self.inbound_call(&call.id, sid, caller)?;
        self.apply_status(&call.id, sid, fields, true)
    }
}
