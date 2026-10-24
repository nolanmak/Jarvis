//! Operator-configured speech providers for ConversationRelay TwiML. Values
//! come only from process environment at startup, never from chat text, tool
//! arguments or model output, and every string is validated to a conservative
//! attribute-safe alphabet before it can reach TwiML.
use thiserror::Error;

/// Default ElevenLabs voice ID with the explicit `flash_v2_5` model suffix.
pub const DEFAULT_ELEVENLABS_VOICE: &str = "UgBBYS2sOqTuMpoF3BR0-flash_v2_5";
/// Fixed product nouns that recognition should bias toward on every call.
pub const HINT_NOUNS: [&str; 6] = ["Jarvis", "Discord", "WhatsApp", "Slack", "Linear", "Notion"];
/// Twilio caps `hints`; we cap well under it so an unexpected name cannot
/// push the attribute past the provider limit.
pub const HINTS_MAX_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttProvider {
    Deepgram,
    Google,
}
impl SttProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deepgram => "Deepgram",
            Self::Google => "Google",
        }
    }
    fn default_model(self) -> &'static str {
        match self {
            Self::Deepgram => "nova-3-general",
            Self::Google => "telephony",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsProvider {
    ElevenLabs,
    Amazon,
    Google,
}
impl TtsProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ElevenLabs => "ElevenLabs",
            Self::Amazon => "Amazon",
            Self::Google => "Google",
        }
    }
    fn default_voice(self) -> Option<&'static str> {
        match self {
            Self::ElevenLabs => Some(DEFAULT_ELEVENLABS_VOICE),
            Self::Amazon => Some("Joanna"),
            Self::Google => None,
        }
    }
}
/// Variants carry no operator input, so Display/Debug cannot echo a bad value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VoiceConfigError {
    #[error("unknown speech-to-text provider; allowed: Deepgram, Google")]
    UnknownSttProvider,
    #[error("unknown text-to-speech provider; allowed: ElevenLabs, Amazon, Google")]
    UnknownTtsProvider,
    #[error("speech-to-text model must match ^[A-Za-z0-9._-]{{1,96}}$")]
    InvalidSttModel,
    #[error("text-to-speech voice must match ^[A-Za-z0-9._-]{{1,96}}$")]
    InvalidVoice,
    #[error("text-to-speech voice is required for this provider")]
    VoiceRequired,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceConfig {
    stt_provider: SttProvider,
    stt_model: String,
    tts_provider: TtsProvider,
    tts_voice: String,
}
impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            stt_provider: SttProvider::Deepgram,
            stt_model: SttProvider::Deepgram.default_model().into(),
            tts_provider: TtsProvider::ElevenLabs,
            tts_voice: DEFAULT_ELEVENLABS_VOICE.into(),
        }
    }
}
fn attribute_token(value: &str) -> bool {
    (1..=96).contains(&value.len())
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}
impl VoiceConfig {
    /// Builds from the `AUGMENTAGENT_PHONE_STT_*` / `_TTS_*` variables. Empty
    /// or whitespace-only values count as unset. Model and voice default per
    /// provider; Google TTS has no vetted default voice and must be explicit.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, VoiceConfigError> {
        let get = |key| get(key).filter(|v| !v.trim().is_empty());
        let stt_provider = match get("AUGMENTAGENT_PHONE_STT_PROVIDER").as_deref() {
            None | Some("Deepgram") => SttProvider::Deepgram,
            Some("Google") => SttProvider::Google,
            Some(_) => return Err(VoiceConfigError::UnknownSttProvider),
        };
        let tts_provider = match get("AUGMENTAGENT_PHONE_TTS_PROVIDER").as_deref() {
            None | Some("ElevenLabs") => TtsProvider::ElevenLabs,
            Some("Amazon") => TtsProvider::Amazon,
            Some("Google") => TtsProvider::Google,
            Some(_) => return Err(VoiceConfigError::UnknownTtsProvider),
        };
        let stt_model = get("AUGMENTAGENT_PHONE_STT_MODEL")
            .unwrap_or_else(|| stt_provider.default_model().into());
        if !attribute_token(&stt_model) {
            return Err(VoiceConfigError::InvalidSttModel);
        }
        let tts_voice = match get("AUGMENTAGENT_PHONE_TTS_VOICE") {
            Some(voice) => voice,
            None => tts_provider
                .default_voice()
                .ok_or(VoiceConfigError::VoiceRequired)?
                .into(),
        };
        if !attribute_token(&tts_voice) {
            return Err(VoiceConfigError::InvalidVoice);
        }
        Ok(Self {
            stt_provider,
            stt_model,
            tts_provider,
            tts_voice,
        })
    }
    pub fn stt_provider(&self) -> SttProvider {
        self.stt_provider
    }
    pub fn stt_model(&self) -> &str {
        &self.stt_model
    }
    pub fn tts_provider(&self) -> TtsProvider {
        self.tts_provider
    }
    pub fn tts_voice(&self) -> &str {
        &self.tts_voice
    }
}
/// Comma-separated recognition hints: the owner's first name (when a stored
/// profile provides one) followed by the fixed product nouns. The name is
/// reduced to letters, digits, spaces, apostrophes and hyphens so it cannot
/// smuggle separators; the caller XML-escapes the result for the attribute.
pub fn hints(owner_first_name: Option<&str>) -> String {
    let name: String = owner_first_name
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '\'' | '-'))
        .collect::<String>()
        .trim()
        .to_owned();
    let mut parts: Vec<&str> = Vec::with_capacity(HINT_NOUNS.len() + 1);
    if !name.is_empty() && name.chars().count() <= 64 {
        parts.push(&name);
    }
    parts.extend(HINT_NOUNS);
    let joined = parts.join(", ");
    let mut capped: String = joined.chars().take(HINTS_MAX_CHARS).collect();
    capped.truncate(capped.trim_end_matches([',', ' ']).len());
    capped
}
