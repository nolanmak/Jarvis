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
    #[cfg(test)]
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
            #[cfg(test)]
            endpoint: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test(endpoint: String) -> Self {
        let mut config = Self::new("synthetic-key".into()).unwrap();
        config.endpoint = Some(endpoint);
        config
    }
    /// Validate exact session configuration before placing a phone call. This
    /// short probe carries no owner profile, transcript or delegated work.
    pub async fn preflight(&self) -> Result<(), LiveError> {
        let connection = self
            .connect("Connection readiness check. Remain silent and do not delegate any work.")
            .await?;
        match connection.finish().await? {
            crate::live_session::LiveInput::Closed {
                reason: crate::live_session::CloseReason::Requested,
                seconds: Some(_),
            } => Ok(()),
            _ => Err(LiveError::Protocol),
        }
    }
    pub async fn connect_for_call(&self, instructions: &str) -> Result<LiveConnection, LiveError> {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return LiveConnection::connect_call_test(endpoint, &self.api_key, instructions).await;
        }
        LiveConnection::connect_for_call(&self.api_key, instructions).await
    }
    pub async fn connect(&self, instructions: &str) -> Result<LiveConnection, LiveError> {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return LiveConnection::connect_test(endpoint, &self.api_key, instructions).await;
        }
        LiveConnection::connect(&self.api_key, instructions).await
    }
}

#[cfg(test)]
mod tests;
