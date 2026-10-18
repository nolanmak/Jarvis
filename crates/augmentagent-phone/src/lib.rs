//! Owner-bound phone calls shared by interactive chat transports.
//! The agent supplies a request ID, never routing or authentication fields.
use async_trait::async_trait;
use augmentagent_store::{
    phone::{BeginVerification, CallState, PhoneCall, PhoneDestination, PhoneError, ReserveCall},
    Store, SurfaceConversationRef, SurfaceOwnerRef,
};
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }
}

/// Intentionally not Debug/Serialize: TwiML may contain a verification code.
pub struct DialRequest {
    pub destination: PhoneDestination,
    pub twiml: String,
    pub status_callback: String,
    pub max_duration_seconds: i64,
}
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider rejected the call")]
    Rejected,
    #[error("provider outcome is unknown")]
    Uncertain,
}
#[async_trait]
pub trait Telephony: Send + Sync {
    async fn create(&self, request: DialRequest) -> Result<String, ProviderError>;
    async fn hangup(&self, sid: &str) -> Result<(), ProviderError>;
}
#[derive(Debug, Error)]
pub enum Error {
    #[error("phone calling is disabled")]
    Disabled,
    #[error("phone configuration is incomplete or invalid")]
    NotConfigured,
    #[error("owner authorization required")]
    Unauthorized,
    #[error("phone capability has expired")]
    Expired,
    #[error("use the owner's private chat to configure a number")]
    PrivateSetupRequired,
    #[error("invalid phone tool arguments")]
    InvalidArguments,
    #[error(transparent)]
    Store(#[from] PhoneError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotConfigured => "not_configured",
            Self::Unauthorized => "unauthorized",
            Self::Expired => "expired",
            Self::PrivateSetupRequired => "private_setup_required",
            Self::InvalidArguments => "invalid_arguments",
            Self::Store(PhoneError::NotConfigured) => "not_configured",
            Self::Store(PhoneError::AlreadyActive) => "already_active",
            Self::Store(PhoneError::RateLimited) => "rate_limited",
            Self::Store(PhoneError::BudgetExhausted) => "budget_exhausted",
            Self::Store(PhoneError::NotFound) => "not_found",
            Self::Store(PhoneError::Conflict) => "conflict",
            Self::Store(PhoneError::InvalidInput) => "invalid_arguments",
            Self::Store(PhoneError::AcceptanceExpired) => "acceptance_expired",
            Self::Store(_) => "storage_error",
            Self::Provider(ProviderError::Rejected) => "provider_rejected",
            Self::Provider(ProviderError::Uncertain) => "provider_uncertain",
        }
    }
}
/// Explicit operator-owned links. Never construct this from model arguments.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerLink {
    pub surface_owner: SurfaceOwnerRef,
    pub owner_id: String,
}
pub struct PhoneConfig {
    enabled: bool,
    public_base: String,
    verification_key: Vec<u8>,
    links: Vec<OwnerLink>,
}
impl PhoneConfig {
    pub fn new(
        enabled: bool,
        public_base: &str,
        verification_key: &str,
        links: Vec<OwnerLink>,
    ) -> Result<Self, Error> {
        let url = reqwest::Url::parse(public_base).map_err(|_| Error::NotConfigured)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || url.port().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || verification_key.len() < 32
        {
            return Err(Error::NotConfigured);
        }
        for (i, link) in links.iter().enumerate() {
            if link.owner_id.is_empty()
                || link.owner_id.len() > 512
                || links[..i]
                    .iter()
                    .any(|old| old.surface_owner == link.surface_owner)
            {
                return Err(Error::NotConfigured);
            }
        }
        Ok(Self {
            enabled,
            public_base: url.origin().ascii_serialization(),
            verification_key: verification_key.as_bytes().into(),
            links,
        })
    }
}
/// Only the server can construct a capability, after checking stored authority.
/// Do not serialize it into a model prompt or accept it as a JSON tool argument.
pub struct Grant {
    surface_owner: SurfaceOwnerRef,
    owner_id: String,
    conversation: SurfaceConversationRef,
    expires_ms: i64,
    binding_confirmed_ms: i64,
}
pub struct PhoneService {
    store: Arc<Store>,
    provider: Arc<dyn Telephony>,
    clock: Arc<dyn Clock>,
    config: PhoneConfig,
}
impl PhoneService {
    pub fn new(
        store: Arc<Store>,
        provider: Arc<dyn Telephony>,
        clock: Arc<dyn Clock>,
        config: PhoneConfig,
    ) -> Self {
        Self {
            store,
            provider,
            clock,
            config,
        }
    }
    pub fn authorize(
        &self,
        owner: &SurfaceOwnerRef,
        conversation: &SurfaceConversationRef,
    ) -> Result<Grant, Error> {
        if owner.account() != conversation.account() {
            return Err(Error::Unauthorized);
        }
        let link = self
            .config
            .links
            .iter()
            .find(|l| &l.surface_owner == owner)
            .ok_or(Error::Unauthorized)?;
        let binding = self
            .store
            .surface_owner_binding(owner.account())
            .map_err(|_| Error::Unauthorized)?
            .ok_or(Error::Unauthorized)?;
        if binding.owner != *owner
            || !binding.is_control_conversation(conversation.conversation_id())
        {
            return Err(Error::Unauthorized);
        }
        Ok(Grant {
            surface_owner: owner.clone(),
            owner_id: link.owner_id.clone(),
            conversation: conversation.clone(),
            expires_ms: self.clock.now_ms() + 300_000,
            binding_confirmed_ms: binding.confirmed_at_ms,
        })
    }
    fn check(&self, grant: &Grant, private: bool) -> Result<(), Error> {
        if self.clock.now_ms() >= grant.expires_ms {
            return Err(Error::Expired);
        }
        let fresh = self.authorize(&grant.surface_owner, &grant.conversation)?;
        if fresh.owner_id != grant.owner_id
            || fresh.binding_confirmed_ms != grant.binding_confirmed_ms
        {
            return Err(Error::Unauthorized);
        }
        if private {
            let binding = self
                .store
                .surface_owner_binding(grant.surface_owner.account())
                .map_err(|_| Error::Unauthorized)?
                .ok_or(Error::Unauthorized)?;
            if binding
                .direct_conversation()
                .is_none_or(|c| c.conversation_id() != grant.conversation.conversation_id())
            {
                return Err(Error::PrivateSetupRequired);
            }
        }
        Ok(())
    }
    fn enabled(&self) -> Result<(), Error> {
        if self.config.enabled {
            Ok(())
        } else {
            Err(Error::Disabled)
        }
    }
    pub async fn tool(&self, grant: &Grant, name: &str, args: Value) -> Result<Value, Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Start {
            request_id: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Call {
            call_id: String,
        }
        self.check(grant, false)?;
        let call = match name {
            "call_owner" => {
                self.enabled()?;
                let args: Start =
                    serde_json::from_value(args).map_err(|_| Error::InvalidArguments)?;
                let conversation = grant.conversation.storage_key();
                let reserved = self.store.phone_reserve_call(ReserveCall {
                    owner: &grant.owner_id,
                    request_id: &args.request_id,
                    conversation: &conversation,
                    now_ms: self.clock.now_ms(),
                })?;
                if reserved.should_dial {
                    let action = format!(
                        "{}/phone/start/{}",
                        self.config.public_base, reserved.call.id
                    );
                    let twiml = format!(
                        "<Response><Redirect method=\"POST\">{}</Redirect></Response>",
                        xml(&action)
                    );
                    self.dial(&reserved.call, twiml).await?;
                }
                self.store.phone_call(&grant.owner_id, &reserved.call.id)?
            }
            "phone_status" => {
                let args: Call =
                    serde_json::from_value(args).map_err(|_| Error::InvalidArguments)?;
                self.store.phone_call(&grant.owner_id, &args.call_id)?
            }
            "phone_hangup" => {
                let args: Call =
                    serde_json::from_value(args).map_err(|_| Error::InvalidArguments)?;
                let call = self
                    .store
                    .phone_request_stop(&grant.owner_id, &args.call_id)?;
                if !call.state.terminal() {
                    if let Some(sid) = &call.provider_sid {
                        self.provider.hangup(sid).await?;
                    }
                }
                call
            }
            _ => return Err(Error::InvalidArguments),
        };
        serde_json::to_value(call).map_err(|_| Error::InvalidArguments)
    }
    async fn dial(&self, call: &PhoneCall, twiml: String) -> Result<(), Error> {
        // Removal can race an in-flight create. Check before send, and again
        // after binding its SID; a removal during the request then hangs up.
        if self.store.phone_call(&call.owner, &call.id)?.stop_requested {
            return Ok(());
        }
        let request = DialRequest {
            destination: self.store.phone_dial_destination(&call.id)?,
            twiml,
            status_callback: format!("{}/phone/status/{}", self.config.public_base, call.id),
            max_duration_seconds: call.max_duration_seconds,
        };
        match self.provider.create(request).await {
            Ok(sid) => {
                self.store.phone_bind_provider(&call.id, &sid)?;
                let latest = self.store.phone_call(&call.owner, &call.id)?;
                if latest.stop_requested {
                    self.provider.hangup(&sid).await?;
                } else {
                    self.store.phone_provider_event(
                        &call.id,
                        &sid,
                        CallState::Queued,
                        self.clock.now_ms(),
                    )?;
                }
            }
            Err(ProviderError::Uncertain) => self.store.phone_mark_uncertain(&call.id)?,
            Err(ProviderError::Rejected) => {
                self.store
                    .phone_create_rejected(&call.id, self.clock.now_ms())?;
            }
        }
        Ok(())
    }
    fn code_hash(&self, owner: &str, request: &str, code: &str) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.config.verification_key)
            .expect("HMAC accepts arbitrary key sizes");
        for field in [owner, request, code] {
            mac.update(&(field.len() as u64).to_be_bytes());
            mac.update(field.as_bytes());
        }
        mac.finalize().into_bytes().into()
    }
    pub async fn set_number(
        &self,
        grant: &Grant,
        request_id: &str,
        number: &str,
    ) -> Result<Value, Error> {
        self.check(grant, true)?;
        self.enabled()?;
        let code = format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32));
        let hash = self.code_hash(&grant.owner_id, request_id, &code);
        let conversation = grant.conversation.storage_key();
        let reserved = self.store.phone_begin_verification(BeginVerification {
            request: ReserveCall {
                owner: &grant.owner_id,
                request_id,
                conversation: &conversation,
                now_ms: self.clock.now_ms(),
            },
            number,
            code_hash: &hash,
        })?;
        if reserved.should_dial {
            let digits = code
                .chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            self.dial(&reserved.call,format!("<Response><Say>Your Jarvis verification code is {digits}. Again, {digits}.</Say><Hangup/></Response>")).await?;
        }
        serde_json::to_value(self.store.phone_destination_status(&grant.owner_id)?)
            .map_err(|_| Error::InvalidArguments)
    }
    /// Call directly from the private command router, BEFORE history/LLM ingestion.
    pub fn verify_number(&self, grant: &Grant, code: &str) -> Result<bool, Error> {
        self.check(grant, true)?;
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidArguments);
        }
        let Some(request) = self.store.phone_verification_request(&grant.owner_id)? else {
            return Ok(false);
        };
        let hash = self.code_hash(&grant.owner_id, &request, code);
        Ok(self
            .store
            .phone_verify_destination(&grant.owner_id, &hash, self.clock.now_ms())?)
    }
    pub fn number_status(&self, grant: &Grant) -> Result<Value, Error> {
        self.check(grant, true)?;
        serde_json::to_value(self.store.phone_destination_status(&grant.owner_id)?)
            .map_err(|_| Error::InvalidArguments)
    }
    pub async fn remove_number(&self, grant: &Grant) -> Result<(), Error> {
        self.check(grant, true)?;
        for id in self
            .store
            .phone_remove_destination(&grant.owner_id, self.clock.now_ms())?
        {
            let call = self.store.phone_call(&grant.owner_id, &id)?;
            if let Some(sid) = call.provider_sid {
                self.provider.hangup(&sid).await?;
            }
        }
        Ok(())
    }
}
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub mod twilio;
pub mod webhook;

pub mod relay;

impl PhoneService {
    /// Called on startup (restart=true) and every 250 ms thereafter. Provider
    /// duration caps remain effective even if the daemon is no longer running.
    pub async fn maintenance(&self, restart: bool) -> Result<(), Error> {
        let now = self.clock.now_ms();
        for call in self.store.phone_active_calls()? {
            let expired = match (call.state, call.answered_ms) {
                (CallState::AwaitingAcceptance, Some(at)) => now - at >= 15_000,
                (_, Some(at)) => now - at >= call.max_duration_seconds * 1000,
                _ => now - call.created_ms >= 60_000,
            };
            if restart || expired || !self.config.enabled {
                self.store.phone_request_stop(&call.owner, &call.id)?;
                if call.provider_sid.is_none() {
                    self.store.phone_mark_uncertain(&call.id)?;
                }
            }
            if self.store.phone_claim_cleanup(&call.id, now)? {
                if let Some(sid) = &call.provider_sid {
                    // Failed cleanup remains visible and is retried by the
                    // durable bounded schedule. Never create another call.
                    if self.provider.hangup(sid).await.is_ok() {
                        let terminal = if call.answered_ms.is_some() {
                            CallState::Completed
                        } else {
                            CallState::Canceled
                        };
                        self.store.phone_provider_event(
                            &call.id,
                            sid,
                            terminal,
                            self.clock.now_ms(),
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

pub mod ingress;
