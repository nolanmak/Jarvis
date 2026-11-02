//! Operator-owned backend selection. No callback, transcript or tool arguments
//! can set a provider endpoint, credential, model or voice.
use crate::{live_connection::LiveConnection, live_session::LiveError, Error};
pub const BACKEND_SETTING: &str = "AUGMENTAGENT_PHONE_VOICE_BACKEND";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceBackend {
    ConversationRelay,
    OpenAiLive,
}
impl VoiceBackend {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        match get(BACKEND_SETTING).as_deref() {
            None | Some("conversation_relay") => Ok(Self::ConversationRelay),
            Some("openai_live") => Ok(Self::OpenAiLive),
            _ => Err(Error::InvalidSetting(BACKEND_SETTING)),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConversationRelay => "conversation_relay",
            Self::OpenAiLive => "openai_live",
        }
    }
}
// Intentionally no Debug/Serialize: credential material stays private.
pub struct LiveConfig {
    api_key: String,
    voice: crate::live_voice::LiveVoice,
    #[cfg(any(test, feature = "test-support"))]
    endpoint: Option<String>,
}
impl LiveConfig {
    pub fn new(api_key: String) -> Result<Self, Error> {
        if api_key.is_empty()
            || api_key.len() > 8192
            || api_key
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(Error::InvalidSetting("OPENAI_API_KEY"));
        }
        Ok(Self {
            api_key,
            voice: crate::live_voice::LiveVoice::default(),
            #[cfg(any(test, feature = "test-support"))]
            endpoint: None,
        })
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(endpoint: String) -> Self {
        assert!(endpoint.starts_with("ws://127.0.0.1:"), "test endpoint must be loopback");
        let mut config = Self::new("synthetic-key".into()).unwrap();
        config.endpoint = Some(endpoint);
        config
    }
    pub fn with_voice(mut self, voice: crate::live_voice::LiveVoice) -> Self {
        self.voice = voice;
        self
    }
    pub fn voice_label(&self) -> &str {
        self.voice.label()
    }
    pub async fn connect_for_call(&self, instructions: &str) -> Result<LiveConnection, LiveError> {
        self.connect_mode(instructions, true).await
    }
    pub async fn connect(&self, instructions: &str) -> Result<LiveConnection, LiveError> {
        self.connect_mode(instructions, false).await
    }
    async fn connect_mode(
        &self,
        instructions: &str,
        greet: bool,
    ) -> Result<LiveConnection, LiveError> {
        let endpoint = crate::live_session::ENDPOINT;
        #[cfg(any(test, feature = "test-support"))]
        let endpoint = self.endpoint.as_deref().unwrap_or(endpoint);
        LiveConnection::connect_to_mode(
            endpoint,
            &self.api_key,
            instructions,
            greet,
            self.voice.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests;
