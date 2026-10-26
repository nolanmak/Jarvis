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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalUsage {
    Completed(augmentagent_store::phone::ProviderUsage),
    NeverAnswered,
}
#[async_trait]
pub trait Telephony: Send + Sync {
    async fn create(&self, request: DialRequest) -> Result<String, ProviderError>;
    async fn hangup(&self, sid: &str) -> Result<(), ProviderError>;
    /// Read-only final usage recovery. None means no authoritative final result.
    async fn final_usage(&self, _sid: &str) -> Result<Option<FinalUsage>, ProviderError> {
        Ok(None)
    }

    /// Read-only provider evidence tying a candidate SID to this exact local
    /// call's callback URL. No evidence means no association is permitted.
    async fn confirms_origin(&self, _sid: &str, _call_id: &str, _public_base: &str) -> Result<bool, ProviderError> {
        Ok(false)
    }

    /// Persisted answer state is a hint, not a fresh provider read. Providers
    /// must handle a call advancing while its cancellation is being requested.
    async fn hangup_known_state(&self, sid: &str, _answered: bool) -> Result<(), ProviderError> {
        self.hangup(sid).await
    }
}
#[derive(Debug, Error)]
pub enum Error {
    #[error("phone calling is disabled")]
    Disabled,
    #[error("phone configuration is incomplete or invalid")]
    NotConfigured,
    /// A named operator setting is out of range or malformed. Carries only the
    /// variable name, never its value.
    #[error("phone setting {0} is invalid")]
    InvalidSetting(&'static str),
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
    #[error("phone voice configuration is invalid: {0}")]
    Voice(#[from] voice::VoiceConfigError),
    #[error("phone Live provider: {0}")]
    Live(#[from] live_session::LiveError),
    #[error("phone silence policy is invalid: {0}")]
    Silence(#[from] silence::SilenceConfigError),
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NotConfigured | Self::InvalidSetting(_) => "not_configured",
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
            Self::Voice(_) | Self::Silence(_) => "not_configured",
            Self::Live(live_session::LiveError::Authentication | live_session::LiveError::Access | live_session::LiveError::Configuration) => "not_configured",
            Self::Live(_) => "provider_unavailable",
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
    voice: voice::VoiceConfig,
    live: Option<live_config::LiveConfig>,
    owner_first_name: Option<String>,
    debug_events: bool,
    barge_in: relay::BargeIn,
    fast: Option<fast::FastLayerConfig>,
    preemptive: bool,
    proposals: proposals::ProposalPolicy,
    silence: silence::SilencePolicy,
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
            voice: voice::VoiceConfig::default(),
            live: None,
            owner_first_name: None,
            debug_events: false,
            barge_in: relay::BargeIn::default(),
            fast: None,
            preemptive: false,
            proposals: proposals::ProposalPolicy::default(),
            silence: silence::SilencePolicy::default(),
        })
    }
    pub fn with_live(mut self, live: Option<live_config::LiveConfig>) -> Self {
        self.live = live;
        if self.live.is_some() {
            self.fast = None;
            self.preemptive = false;
        }
        self
    }
    /// Quiet hours and owner time zone for agent-proposed calls (#1553).
    pub fn with_proposals(mut self, proposals: proposals::ProposalPolicy) -> Self {
        self.proposals = proposals;
        self
    }
    /// Operator-validated speech providers. Only startup configuration may call this.
    pub fn with_voice(mut self, voice: voice::VoiceConfig) -> Self {
        self.voice = voice;
        self
    }
    /// Owner first name, used only as a recognition hint. `config::Settings::build`
    /// supplies the stored `first_name` profile fact (#1552) at startup.
    pub fn with_owner_first_name(mut self, first_name: Option<String>) -> Self {
        self.owner_first_name = first_name;
        self
    }
    /// Request ConversationRelay debug events (tokens played, speaker events)
    /// so #1551 can measure audible latency. Off unless explicitly enabled.
    pub fn with_debug_events(mut self, enabled: bool) -> Self {
        self.debug_events = enabled;
        self
    }
    /// Silence check-in and automatic-end thresholds (#1556).
    pub fn with_silence(mut self, silence: silence::SilencePolicy) -> Self {
        self.silence = silence;
        self
    }
    /// Backchannel filter and false-interruption handling (#1550).
    pub fn with_barge_in(mut self, barge_in: relay::BargeIn) -> Self {
        self.barge_in = barge_in;
        self
    }
    /// Answer utterances with the fast conversational layer (#1546).
    pub fn with_fast_layer(mut self, fast: Option<fast::FastLayerConfig>) -> Self {
        self.fast = fast;
        self
    }
    /// Preemptive generation on interim transcripts (#1555). Takes effect only
    /// together with the fast layer.
    pub fn with_preemptive(mut self, enabled: bool) -> Self {
        self.preemptive = enabled;
        self
    }
}
/// Only the server can construct a capability, after checking stored authority.
/// Do not serialize it into a model prompt or accept it as a JSON tool argument.
pub struct Grant {
    surface_owner: SurfaceOwnerRef,
    owner_id: String,
    conversation: SurfaceConversationRef,
    expires_ms: i64,
    binding_revision: i64,
}
pub struct PhoneService {
    store: Arc<Store>,
    provider: Arc<dyn Telephony>,
    clock: Arc<dyn Clock>,
    config: PhoneConfig,
    stopping: std::sync::atomic::AtomicBool,
    cleanup_inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Whether this service holds an open call-proposal answer window (#1553).
    consent_window: std::sync::atomic::AtomicBool,
    dispatch_ready: tokio::sync::Notify,
}
impl Drop for PhoneService {
    fn drop(&mut self) {
        self.set_consent_window(false);
    }
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
            stopping: std::sync::atomic::AtomicBool::new(false),
            cleanup_inflight: Arc::new(std::sync::Mutex::new(Default::default())),
            consent_window: std::sync::atomic::AtomicBool::new(false),
            dispatch_ready: tokio::sync::Notify::new(),
        }
    }
    /// Explicit operator-configured identities, for installing transport adapters
    /// before the first private control conversation has been provisioned.
    pub fn barge_in(&self) -> &relay::BargeIn {
        &self.config.barge_in
    }
    pub fn debug_events(&self) -> bool {
        self.config.debug_events
    }
    /// Operator selection for the next reserved call.
    pub fn voice_backend(&self) -> live_config::VoiceBackend {
        if self.config.live.is_some() {
            live_config::VoiceBackend::OpenAiLive
        } else {
            live_config::VoiceBackend::ConversationRelay
        }
    }
    /// Legacy text fast layer; always absent for Live sessions.
    pub fn fast_layer(&self) -> Option<&fast::FastLayerConfig> {
        if self.config.live.is_some() {
            return None;
        }
        self.config.fast.as_ref()
    }
    /// `AUGMENTAGENT_PHONE_PREEMPTIVE=1` on the fast layer (#1555): TwiML
    /// requests interim prompts and the relay may speculate on them.
    pub fn preemptive(&self) -> bool {
        self.config.live.is_none() && self.config.preemptive && self.config.fast.is_some()
    }
    /// Silence check-in and automatic-end thresholds (#1556).
    pub fn silence_policy(&self) -> silence::SilencePolicy {
        self.config.silence
    }
    pub fn linked_surface_owners(&self) -> Vec<SurfaceOwnerRef> {
        self.config
            .links
            .iter()
            .map(|link| link.surface_owner.clone())
            .collect()
    }

    /// Called only by an authenticated Discord DM adapter on explicit number
    /// setup. Never restores an account that has previously held authority.
    pub fn initialize_discord_private_owner(
        &self,
        owner: &SurfaceOwnerRef,
        conversation: &SurfaceConversationRef,
    ) -> Result<(), Error> {
        self.enabled()?;
        if owner.account().platform().as_str() != "discord"
            || !owner
                .account()
                .account_id()
                .strip_prefix("dm:")
                .is_some_and(|id| id.parse::<u64>().is_ok_and(|n| n > 0))
            || !self
                .config
                .links
                .iter()
                .any(|link| link.surface_owner == *owner)
        {
            return Err(Error::Unauthorized);
        }
        self.store
            .initialize_private_surface_owner(owner, conversation, self.clock.now_ms())
            .map_err(|_| Error::Unauthorized)?;
        self.authorize(owner, conversation)?;
        Ok(())
    }

    /// Trusted WhatsApp adapter only, after authenticated sidecar identity and
    /// self-message checks. Explicit operator links are required before setup.
    /// Previously revoked bindings cannot be recreated by chat.
    pub fn initialize_whatsapp_private_owner(
        &self,
        owner: &SurfaceOwnerRef,
        conversation: &SurfaceConversationRef,
    ) -> Result<(), Error> {
        self.enabled()?;
        let number = owner.account().account_id().strip_prefix("device:");
        if owner.account().platform().as_str() != "whatsapp"
            || !number.is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            || owner.sender_id() != format!("{}@s.whatsapp.net", number.unwrap_or_default())
            || conversation.conversation_id() != owner.sender_id()
            || !self
                .config
                .links
                .iter()
                .any(|link| link.surface_owner == *owner)
        {
            return Err(Error::Unauthorized);
        }
        self.store
            .initialize_private_surface_owner(owner, conversation, self.clock.now_ms())
            .map_err(|_| Error::Unauthorized)?;
        self.authorize(owner, conversation)?;
        Ok(())
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
            binding_revision: binding.revision,
        })
    }
    fn check(&self, grant: &Grant, private: bool) -> Result<(), Error> {
        if self.clock.now_ms() >= grant.expires_ms {
            return Err(Error::Expired);
        }
        let fresh = self.authorize(&grant.surface_owner, &grant.conversation)?;
        if fresh.owner_id != grant.owner_id || fresh.binding_revision != grant.binding_revision {
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
        if self.config.enabled && !self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
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
                self.finish_call_reservation(grant, reserved).await?
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
                    self.maintenance(false).await?;
                }
                self.store.phone_call(&grant.owner_id, &args.call_id)?
            }
            _ => return Err(Error::InvalidArguments),
        };
        let mut receipt = serde_json::to_value(&call).map_err(|_| Error::InvalidArguments)?;
        if name == "phone_status" {
            receipt["maintenance"] = serde_json::to_value(
                self.store.phone_maintenance_status(&grant.owner_id, &call.id)?
            ).map_err(|_| Error::InvalidArguments)?;
        }
        Ok(receipt)
    }
    async fn finish_call_reservation(&self, grant: &Grant, reserved: augmentagent_store::phone::CallReservation) -> Result<PhoneCall, Error> {
        if reserved.should_dial {
            self.store.phone_set_voice_backend(&reserved.call.id, self.voice_backend().as_str())?;
            let action = format!(
                "{}/phone/start/{}",
                self.config.public_base, reserved.call.id
            );
            let twiml = format!(
                "<Response><Redirect method=\"POST\">{}</Redirect></Response>",
                xml(&action)
            );
            self.store.phone_register_origin(
                &reserved.call.id,
                &augmentagent_store::phone::PhoneOrigin {
                    owner: grant.surface_owner.clone(),
                    conversation: grant.conversation.clone(),
                },
                grant.binding_revision,
            )?;
            if let Some(live) = &self.config.live {
                if let Err(error) = live.preflight().await {
                    self.store.phone_create_rejected(&reserved.call.id, self.clock.now_ms())?;
                    return Err(error.into());
                }
                // Authorization can change while the readiness probe is in flight.
                if let Err(error) = self.check(grant, false) {
                    self.store.phone_create_rejected(&reserved.call.id, self.clock.now_ms())?;
                    return Err(error);
                }
            }
            self.dial(&reserved.call, twiml).await?;
        }
        Ok(self.store.phone_call(&grant.owner_id, &reserved.call.id)?)
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
                    self.maintenance(false).await?;
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
        self.store
            .phone_remove_destination(&grant.owner_id, self.clock.now_ms())?;
        self.maintenance(false).await?;
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
pub mod speech;
pub mod voice;

impl PhoneService {
    /// Snapshot and claim due cleanup without waiting for provider HTTP. The
    /// runtime excludes in-flight IDs so a slow request cannot spend all retries.
    fn cleanup_due(&self, restart: bool) -> Result<Vec<CleanupCall>, Error> {
        let now = self.clock.now_ms();
        let mut due = Vec::new();
        for call in self.store.phone_active_calls()? {
            let expired = match (call.state, call.answered_ms) {
                (CallState::AwaitingAcceptance, Some(at)) => now - at >= 15_000,
                (_, Some(at)) => now - at >= call.max_duration_seconds * 1000,
                _ => now - call.created_ms >= 60_000,
            };
            if restart
                || expired
                || !self.config.enabled
                || self.stopping.load(std::sync::atomic::Ordering::SeqCst)
                || self.store.phone_budget_remaining_ms(&call.owner, now)? == 0
            {
                self.store.phone_request_stop(&call.owner, &call.id)?;
                if call.provider_sid.is_none() {
                    self.store.phone_mark_uncertain(&call.id)?;
                }
            }
            let mut inflight = self
                .cleanup_inflight
                .lock()
                .expect("phone cleanup poisoned");
            if !inflight.contains(&call.id) && self.store.phone_claim_cleanup(&call.id, now)? {
                inflight.insert(call.id.clone());
                due.push(CleanupCall {
                    call,
                    inflight: self.cleanup_inflight.clone(),
                });
            }
        }
        for call in self.store.phone_usage_reconciliation_due(now)? {
            let mut inflight = self.cleanup_inflight.lock().expect("phone cleanup poisoned");
            // Hangups have priority; historical usage reads must not create an
            // unbounded set of HTTP jobs while a provider is slow.
            if inflight.len() >= 8 { break; }
            if !inflight.contains(&call.id) && self.store.phone_claim_usage_reconciliation(&call.id, now)? {
                inflight.insert(call.id.clone());
                due.push(CleanupCall { call, inflight: self.cleanup_inflight.clone() });
            }
        }
        Ok(due)
    }
    async fn cleanup_call(&self, cleanup: CleanupCall) -> Result<(), Error> {
        let call = &cleanup.call;
        if let Some(sid) = &call.provider_sid {
            if call.state.terminal() {
                if let Ok(Ok(Some(result))) = tokio::time::timeout(
                    std::time::Duration::from_secs(5), self.provider.final_usage(sid)).await {
                    let usage = match result {
                        FinalUsage::Completed(usage) => usage,
                        FinalUsage::NeverAnswered => {
                            if self.store.phone_usage(&call.id)?.is_none() {
                                if let Err(error) = self.store.phone_confirm_never_answered(&call.id, sid, self.clock.now_ms()) {
                                    // An answer/final callback may win during provider I/O.
                                    // Conflicting evidence must retain the existing budget.
                                    if !matches!(error, PhoneError::Conflict) { return Err(error.into()); }
                                }
                            }
                            return Ok(());
                        },
                    };
                    // A signed callback may have won while REST was in flight.
                    // Preserve that immutable evidence instead of overwriting it.
                    if self.store.phone_usage(&call.id)?.is_none() {
                        self.store.phone_provider_event_with_usage(&call.id, sid, call.state, self.clock.now_ms(), Some(usage))?;
                    }
                }
                return Ok(());
            }
            // A failed or timed-out attempt stays durably pending for the next
            // bounded retry. A terminal provider callback never regresses.
            if matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(32),
                    self.provider
                        .hangup_known_state(sid, call.answered_ms.is_some())
                )
                .await,
                Ok(Ok(()))
            ) {
                let terminal = if call.answered_ms.is_some() {
                    CallState::Completed
                } else {
                    CallState::Canceled
                };
                self.store
                    .phone_provider_event(&call.id, sid, terminal, self.clock.now_ms())?;
            }
        }
        Ok(())
    }
    /// One maintenance pass for explicit cleanup and tests. The continuous
    /// runtime keeps scanning while these provider requests are in flight.
    pub async fn maintenance(&self, restart: bool) -> Result<(), Error> {
        let calls = self.cleanup_due(restart)?;
        let outcomes =
            futures::future::join_all(calls.into_iter().map(|call| self.cleanup_call(call))).await;
        for outcome in outcomes {
            outcome?;
        }
        Ok(())
    }
}

pub mod ingress;

impl PhoneService {
    /// Revalidate the original transport owner before each phone turn. A call
    /// cannot inherit permissions from a different conversation or account.
    pub fn authorize_call(&self, call_id: &str) -> Result<Grant, Error> {
        let call = self.store.phone_provider_call(call_id)?;
        if call.state != CallState::Connected || call.stop_requested || call.verification {
            return Err(Error::Unauthorized);
        }
        self.authorize_call_origin(&call)
    }

    /// Owner authority for the call's origin, without requiring the call to
    /// still be connected. Dispatched work (#1546) outlives its call.
    pub fn authorize_call_origin(&self, call: &PhoneCall) -> Result<Grant, Error> {
        let origin = self.store.phone_origin(&call.id)?;
        let grant = self.authorize(&origin.owner, &origin.conversation)?;
        if grant.owner_id != call.owner
            || grant.conversation.storage_key() != call.conversation
            || self.store.phone_origin_revision(&call.id)? != grant.binding_revision
        {
            return Err(Error::Unauthorized);
        }
        Ok(grant)
    }
}

pub mod commands;
pub mod context;
pub mod tools;

impl PhoneService {
    pub fn latest_call(&self, grant: &Grant) -> Result<Option<PhoneCall>, Error> {
        self.check(grant, false)?;
        Ok(self.store.phone_latest_call(&grant.owner_id)?)
    }
}

pub mod mcp;

pub mod runtime;

/// Shared by watchdog, disconnect and explicit maintenance, including while a
/// cleanup future is cancelled. Dropping a job releases only the local lease;
/// the durable attempt/backoff remains recorded.
struct CleanupCall {
    call: PhoneCall,
    inflight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}
impl Drop for CleanupCall {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .expect("phone cleanup poisoned")
            .remove(&self.call.id);
    }
}

pub mod config;

pub mod router;

pub mod recovery;

pub mod metrics;

pub mod fast;

pub mod memory;
pub mod end_call;
/// Preemptive generation on interim transcripts (#1555).
pub mod speculation;

pub mod proposals;
pub mod silence;

pub mod live_transport;

pub mod live_session;

pub mod live_connection;

pub mod live_context;

pub mod live_call;

pub mod live_config;
