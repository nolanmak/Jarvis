//! Operator-selected voice, frozen at session creation.
use crate::Error;
use serde_json::{json, Value};
pub const VOICE_SETTING: &str = "AUGMENTAGENT_PHONE_LIVE_VOICE";
pub const DESCRIPTION_SETTING: &str = "AUGMENTAGENT_PHONE_LIVE_VOICE_DESCRIPTION";
// No Debug/Serialize: custom IDs and descriptions are operator-private.
#[derive(Clone)]
pub struct LiveVoice {
    name: String,
    description: Option<String>,
}
impl Default for LiveVoice {
    fn default() -> Self {
        Self {
            name: "gleam".into(),
            description: None,
        }
    }
}
impl LiveVoice {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self, Error> {
        Self::parse(
            get(VOICE_SETTING).as_deref().unwrap_or("gleam"),
            get(DESCRIPTION_SETTING).as_deref(),
        )
    }
    pub fn parse(name: &str, description: Option<&str>) -> Result<Self, Error> {
        let preset = matches!(name, "gleam" | "willow" | "quartz" | "delta");
        let custom = name.strip_prefix("voice_").is_some_and(|suffix| {
            !suffix.is_empty()
                && name.len() <= 256
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
        if !preset && !custom {
            return Err(Error::InvalidSetting(VOICE_SETTING));
        }
        if (preset && description.is_some())
            || (custom
                && !description.is_some_and(|d| {
                    !d.trim().is_empty() && d.len() <= 2000 && !d.chars().any(char::is_control)
                }))
        {
            return Err(Error::InvalidSetting(DESCRIPTION_SETTING));
        }
        Ok(Self {
            name: name.into(),
            description: description.map(str::to_owned),
        })
    }
    pub fn label(&self) -> &str {
        if self.description.is_some() {
            "custom"
        } else {
            &self.name
        }
    }
    pub(crate) fn wire(&self) -> Value {
        if self.description.is_some() {
            json!({"id":self.name})
        } else {
            json!(self.name)
        }
    }
    pub(crate) fn instructions(&self, base: &str) -> String {
        match &self.description {
            Some(description) => format!("{base}\nVoice description: {description}"),
            None => base.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_session::{LiveError, LiveSession};
    use serde_json::json;

    #[test]
    fn selection_validates_and_preserves_default() {
        assert_eq!(
            LiveVoice::from_lookup(&|_| None).unwrap().wire(),
            json!("gleam")
        );
        for name in ["gleam", "willow", "quartz", "delta"] {
            let voice = LiveVoice::parse(name, None).unwrap();
            assert_eq!(voice.wire(), json!(name));
            assert_eq!(voice.label(), name);
        }
        for name in ["", "Mark", "https://example.org", "voice_", "voice_x\n"] {
            assert!(LiveVoice::parse(name, None).is_err());
        }
        assert!(LiveVoice::parse("gleam", Some("unused")).is_err());
        assert!(LiveVoice::parse("voice_example", None).is_err());
        assert!(LiveVoice::parse("voice_example", Some(&"x".repeat(2001))).is_err());
    }

    #[test]
    fn custom_voice_is_private_and_sent_as_object_with_description() {
        let voice =
            LiveVoice::parse("voice_example", Some("Warm adult feminine American voice.")).unwrap();
        assert_eq!(voice.label(), "custom");
        let mut session = LiveSession::with_voice("Be helpful.", voice, true).unwrap();
        let event = session.start().unwrap();
        assert_eq!(
            event["session"]["audio"]["output"]["voice"],
            json!({"id":"voice_example"})
        );
        assert!(event["session"]["instructions"]
            .as_str()
            .unwrap()
            .contains("Warm adult feminine"));
        assert!(event["session"]["input"].is_array());
        let mut confirmation = event["session"].clone();
        confirmation["id"] = json!("synthetic");
        confirmation["audio"]["output"]["voice"] = json!("gleam");
        assert!(matches!(
            session.receive(&json!({"type":"session.started","session":confirmation}).to_string()),
            Err(LiveError::Configuration)
        ));
        assert_eq!(session.state(), crate::live_session::LiveState::Failed);
        let mut session = LiveSession::with_voice(
            "Be helpful.",
            LiveVoice::parse("voice_example", Some("Warm adult feminine American voice.")).unwrap(),
            true,
        )
        .unwrap();
        session.start().unwrap();
        confirmation["audio"]["output"]["voice"] = json!({"id":"voice_example"});
        assert!(session
            .receive(&json!({"type":"session.started","session":confirmation}).to_string())
            .is_ok());
    }

    #[test]
    fn selection_is_frozen_and_provider_must_confirm_it() {
        for name in ["gleam", "willow", "quartz", "delta"] {
            for greet in [false, true] {
                let voice = LiveVoice::parse(name, None).unwrap();
                let mut session = LiveSession::with_voice("Be helpful.", voice, greet).unwrap();
                let start = session.start().unwrap();
                assert_eq!(start["session"]["audio"]["output"]["voice"], name);
                assert_eq!(start["session"]["input"].is_array(), greet);
                let mut confirmation = start["session"].clone();
                confirmation["id"] = json!("synthetic");
                let event = json!({"type":"session.started","session":confirmation});
                assert!(session.receive(&event.to_string()).is_ok());
                let mut wrong = LiveSession::with_voice(
                    "Be helpful.",
                    LiveVoice::parse(name, None).unwrap(),
                    greet,
                )
                .unwrap();
                wrong.start().unwrap();
                let mut event = event.clone();
                event["session"]["audio"]["output"]["voice"] = json!("wrong");
                assert!(matches!(
                    wrong.receive(&event.to_string()),
                    Err(LiveError::Configuration)
                ));
            }
        }
    }
}
