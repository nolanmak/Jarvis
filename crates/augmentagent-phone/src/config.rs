//! Explicit operator configuration. Destination numbers belong to the verified
//! owner settings store, never this environment configuration.
use crate::{
    relay::{BargeIn, DEFAULT_BACKCHANNELS},
    twilio::TwilioClient,
    voice::VoiceConfig,
    webhook::WebhookAuth,
    Error, OwnerLink, PhoneConfig, PhoneService, SystemClock,
};
use augmentagent_store::{Store, SurfaceAccountRef, SurfaceOwnerRef, SurfacePlatform};
use serde::Deserialize;
use std::{net::SocketAddr, sync::Arc};

/// Barge-in settings (#1550); see `docs/phone/runtime.md`.
pub const BARGE_MIN_WORDS: &str = "AUGMENTAGENT_PHONE_BARGE_MIN_WORDS";
pub const BACKCHANNEL_PHRASES: &str = "AUGMENTAGENT_PHONE_BACKCHANNEL_PHRASES";
pub const FALSE_INTERRUPT_MS: &str = "AUGMENTAGENT_PHONE_FALSE_INTERRUPT_MS";
pub const RESUME_FALSE_INTERRUPT: &str = "AUGMENTAGENT_PHONE_RESUME_FALSE_INTERRUPT";

fn barge_in(get: &impl Fn(&str) -> Option<String>) -> Result<BargeIn, Error> {
    let number = |key: &'static str, default: u64| match get(key) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| Error::InvalidSetting(key)),
    };
    let min_words = usize::try_from(number(BARGE_MIN_WORDS, 3)?)
        .map_err(|_| Error::InvalidSetting(BARGE_MIN_WORDS))?;
    let phrases = match get(BACKCHANNEL_PHRASES) {
        None => DEFAULT_BACKCHANNELS.iter().map(|s| (*s).into()).collect(),
        Some(v) if v.len() <= 4_096 => v.split(',').map(str::to_owned).collect(),
        Some(_) => return Err(Error::InvalidSetting(BACKCHANNEL_PHRASES)),
    };
    let resume = match get(RESUME_FALSE_INTERRUPT).as_deref() {
        None | Some("1") => true,
        Some("0") => false,
        _ => return Err(Error::InvalidSetting(RESUME_FALSE_INTERRUPT)),
    };
    BargeIn::new(
        min_words,
        phrases,
        number(FALSE_INTERRUPT_MS, 2_000)?,
        resume,
    )
}

// Intentionally no Debug/Serialize: provider credentials and verification key.
pub struct Settings {
    bind: SocketAddr,
    phone: PhoneConfig,
    provider: Arc<TwilioClient>,
    auth: Arc<WebhookAuth>,
    voice: VoiceConfig,
}
pub struct ConfiguredPhone {
    pub bind: SocketAddr,
    pub service: Arc<PhoneService>,
    pub auth: Arc<WebhookAuth>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Link {
    platform: String,
    account: String,
    surface_owner: String,
    owner: String,
}
impl Settings {
    pub fn from_env() -> Result<Option<Self>, Error> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }
    /// Injectable lookup avoids changing process environment in tests. No value
    /// is copied into errors or logs; malformed enabled configuration fails closed.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, Error> {
        match get("AUGMENTAGENT_PHONE_ENABLED").as_deref() {
            None | Some("0") => return Ok(None),
            Some("1") => {}
            _ => return Err(Error::NotConfigured),
        }
        let required = |key| {
            get(key)
                .filter(|s| !s.trim().is_empty())
                .ok_or(Error::NotConfigured)
        };
        let bind: SocketAddr = get("AUGMENTAGENT_PHONE_BIND")
            .unwrap_or_else(|| "127.0.0.1:8878".into())
            .parse()
            .map_err(|_| Error::NotConfigured)?;
        if !bind.ip().is_loopback() || bind.port() == 0 {
            return Err(Error::NotConfigured);
        }
        let raw_links = required("AUGMENTAGENT_PHONE_OWNER_LINKS")?;
        if raw_links.len() > 32_768 {
            return Err(Error::NotConfigured);
        }
        let specs: Vec<Link> =
            serde_json::from_str(&raw_links).map_err(|_| Error::NotConfigured)?;
        if specs.is_empty() || specs.len() > 64 {
            return Err(Error::NotConfigured);
        }
        let links = specs
            .into_iter()
            .map(|l| {
                let platform =
                    SurfacePlatform::new(l.platform).map_err(|_| Error::NotConfigured)?;
                let account = SurfaceAccountRef::new(platform, l.account)
                    .map_err(|_| Error::NotConfigured)?;
                let surface_owner = SurfaceOwnerRef::new(account, l.surface_owner)
                    .map_err(|_| Error::NotConfigured)?;
                Ok(OwnerLink {
                    surface_owner,
                    owner_id: l.owner,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let voice = VoiceConfig::from_lookup(&get)?;
        // Relay debug events (#1551): explicit on/off; absent defaults on for
        // non-production (debug) builds only.
        let debug_events = match get("AUGMENTAGENT_PHONE_DEBUG_EVENTS").as_deref() {
            None => cfg!(debug_assertions),
            Some("1") => true,
            Some("0") => false,
            _ => return Err(Error::NotConfigured),
        };
        // #1555: explicit on/off, default off; inert without the fast layer.
        let preemptive = match get("AUGMENTAGENT_PHONE_PREEMPTIVE").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            _ => return Err(Error::NotConfigured),
        };
        let silence = crate::silence::SilencePolicy::from_lookup(&get)?;
        let public_url = required("AUGMENTAGENT_PHONE_PUBLIC_URL")?;
        let fast = crate::fast::FastLayerConfig::from_lookup(&get, &public_url)?;
        let phone = PhoneConfig::new(
            true,
            &public_url,
            &required("AUGMENTAGENT_PHONE_VERIFICATION_KEY")?,
            links,
        )?
        .with_voice(voice.clone())
        .with_debug_events(debug_events)
        .with_silence(silence)
        .with_barge_in(barge_in(&get)?)
        .with_fast_layer(fast)
        .with_preemptive(preemptive)
        .with_proposals(crate::proposals::ProposalPolicy::from_lookup(&get)?);
        let account = required("TWILIO_ACCOUNT_SID")?;
        let token = required("TWILIO_AUTH_TOKEN")?;
        let provider = Arc::new(TwilioClient::new(
            &account,
            &token,
            &required("TWILIO_PHONE_NUMBER")?,
        )?);
        let auth = Arc::new(WebhookAuth::new(&account, &token));
        Ok(Some(Self {
            bind,
            phone,
            provider,
            auth,
            voice,
        }))
    }
    /// Validated silence thresholds (#1556).
    pub fn silence(&self) -> crate::silence::SilencePolicy {
        self.phone.silence
    }
    /// Validated speech providers as they will be emitted in TwiML.
    pub fn voice(&self) -> &VoiceConfig {
        &self.voice
    }
    pub fn build(mut self, store: Arc<Store>) -> Result<ConfiguredPhone, Error> {
        // #1552: the stored first-name fact (learned from earlier calls) is
        // the recognition hint. One hint per deployment, so only when every
        // link names the same owner; read once at startup.
        let mut owners = self.phone.links.iter().map(|l| l.owner_id.as_str());
        if let Some(owner) = owners.next().filter(|o| owners.all(|other| other == *o)) {
            let name = store
                .phone_owner_first_name(owner)
                .map_err(|_| Error::NotConfigured)?;
            if name.is_some() {
                self.phone = self.phone.with_owner_first_name(name);
            }
        }
        Ok(ConfiguredPhone {
            bind: self.bind,
            service: Arc::new(PhoneService::new(
                store,
                self.provider,
                Arc::new(SystemClock),
                self.phone,
            )),
            auth: self.auth,
        })
    }
}
