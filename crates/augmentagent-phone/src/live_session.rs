//! Primary Live WebSocket lifecycle. This module never owns dialing authority,
//! credentials or endpoints. The runtime must enforce `deadline()` even when
//! the peer sends no frames, and close its socket on any terminal state.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;

pub const MODEL: &str = "gpt-live-1";
pub const VOICE: &str = "gleam";
pub const ENDPOINT: &str = "wss://api.openai.com/v1/live/sessions";
const MAX_AUDIO: usize = 16_000;
const MAX_EVENT: usize = 32_768;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    Created,
    Starting,
    Active,
    Closing,
    Closed,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LiveError {
    #[error("invalid Live session state")]
    State,
    #[error("OpenAI project API credential required or rejected")]
    Authentication,
    #[error("OpenAI project lacks Live access")]
    Access,
    #[error("OpenAI Live rate limit reached")]
    RateLimit,
    #[error("OpenAI Live unavailable")]
    Unavailable,
    #[error("Live configuration rejected")]
    Configuration,
    #[error("invalid Live protocol event")]
    Protocol,
    #[error("Live capacity exceeded")]
    Capacity,
    #[error("Live startup timed out")]
    StartupTimeout,
    #[error("Live finalization timed out")]
    CloseTimeout,
    #[error("Live connection lost before finalization")]
    ConnectionLost,
}
impl LiveError {
    pub fn http(status: u16) -> Self {
        match status {
            401 => Self::Authentication,
            403 => Self::Access,
            429 => Self::RateLimit,
            400 | 422 => Self::Configuration,
            _ => Self::Unavailable,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Requested,
    Expired,
    Content,
    Remote,
    ConnectionLost,
}
// No Debug implementations on content-bearing session/events.
pub enum LiveInput {
    Ignored,
    Started,
    Audio(Vec<u8>),
    Closed {
        reason: CloseReason,
        seconds: Option<f64>,
    },
}
pub struct LiveSession {
    state: LiveState,
    instructions: String,
    id: Option<String>,
    deadline: Option<Instant>,
}
impl LiveSession {
    pub fn new(instructions: &str) -> Result<Self, LiveError> {
        if instructions.is_empty() || instructions.len() > 16_000 {
            return Err(LiveError::Configuration);
        }
        Ok(Self {
            state: LiveState::Created,
            instructions: instructions.into(),
            id: None,
            deadline: None,
        })
    }
    pub fn state(&self) -> LiveState {
        self.state
    }
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn check_deadline(&mut self) -> Result<(), LiveError> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let error = if self.state == LiveState::Starting {
                LiveError::StartupTimeout
            } else {
                LiveError::CloseTimeout
            };
            self.fail();
            return Err(error);
        }
        Ok(())
    }
    fn fail(&mut self) {
        self.state = LiveState::Failed;
        self.deadline = None;
        self.instructions.clear();
    }
    pub fn disconnected(&mut self) -> Result<(), LiveError> {
        if self.state == LiveState::Closed {
            return Ok(());
        }
        self.fail();
        Err(LiveError::ConnectionLost)
    }
    pub fn start(&mut self) -> Result<Value, LiveError> {
        if self.state != LiveState::Created {
            return Err(LiveError::State);
        }
        self.state = LiveState::Starting;
        self.deadline = Some(Instant::now() + Duration::from_secs(10));
        let event = json!({"type":"session.start","session":{
            "model":MODEL,"instructions":self.instructions,
            "audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":VOICE}},
            "delegation":{"type":"client"}}});
        self.instructions.clear();
        Ok(event)
    }
    pub fn audio(&mut self, audio: &[u8]) -> Result<Value, LiveError> {
        self.check_deadline()?;
        if self.state != LiveState::Active {
            return Err(LiveError::State);
        }
        if audio.is_empty() || audio.len() > MAX_AUDIO {
            return Err(LiveError::Capacity);
        }
        Ok(json!({"type":"session.input_audio.append","audio":STANDARD.encode(audio)}))
    }
    pub fn close(&mut self) -> Result<Value, LiveError> {
        self.check_deadline()?;
        if self.state != LiveState::Active {
            return Err(LiveError::State);
        }
        self.state = LiveState::Closing;
        self.deadline = Some(Instant::now() + Duration::from_secs(5));
        Ok(json!({"type":"session.close"}))
    }
    pub fn receive(&mut self, text: &str) -> Result<LiveInput, LiveError> {
        self.check_deadline()?;
        let result = self.receive_inner(text);
        if result.is_err() && self.state != LiveState::Closed {
            self.fail();
        }
        result
    }
    fn receive_inner(&mut self, text: &str) -> Result<LiveInput, LiveError> {
        if text.len() > MAX_EVENT {
            return Err(LiveError::Capacity);
        }
        let v: Value = serde_json::from_str(text).map_err(|_| LiveError::Protocol)?;
        let kind = v["type"].as_str().ok_or(LiveError::Protocol)?;
        if self.state == LiveState::Closed {
            return if kind == "session.closed" && v["session"]["id"].as_str() == self.id() {
                Ok(LiveInput::Ignored)
            } else {
                Err(LiveError::State)
            };
        }
        if matches!(self.state, LiveState::Created | LiveState::Failed) {
            return Err(LiveError::State);
        }
        match kind {
            "error" => {
                // Provider message/param/body must never escape into diagnostics.
                let error = &v["error"];
                Err(match error["type"].as_str() {
                    Some("invalid_request_error") => LiveError::Configuration,
                    Some("authentication_error") => LiveError::Authentication,
                    Some("permission_error") => LiveError::Access,
                    Some("rate_limit_error") => LiveError::RateLimit,
                    _ => LiveError::Unavailable,
                })
            }
            "session.started" => {
                if self.state != LiveState::Starting {
                    return Err(LiveError::State);
                }
                let s = &v["session"];
                if s["model"] != MODEL
                    || s["audio"]["format"] != json!({"type":"audio/pcmu","rate":8000})
                    || s["audio"]["output"]["voice"] != VOICE
                    || s["delegation"]["type"] != "client"
                {
                    return Err(LiveError::Configuration);
                }
                let id = s["id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .ok_or(LiveError::Protocol)?;
                self.id = Some(id.into());
                self.state = LiveState::Active;
                self.deadline = None;
                Ok(LiveInput::Started)
            }
            "session.closed" => {
                if self.id.is_none() || v["session"]["id"].as_str() != self.id() {
                    return Err(LiveError::Protocol);
                }
                let reason = match v["reason"].as_str() {
                    Some("close_requested") => CloseReason::Requested,
                    Some("expired") => CloseReason::Expired,
                    Some("content") => CloseReason::Content,
                    Some("remote_hangup") => CloseReason::Remote,
                    Some("connection_lost") => CloseReason::ConnectionLost,
                    _ => return Err(LiveError::Protocol),
                };
                let seconds = v["usage"]["seconds"]
                    .as_f64()
                    .filter(|s| s.is_finite() && *s >= 0.0);
                self.state = LiveState::Closed;
                self.deadline = None;
                Ok(LiveInput::Closed { reason, seconds })
            }
            "session.output_audio.delta" => {
                if self.state == LiveState::Closing {
                    return Ok(LiveInput::Ignored);
                }
                if self.state != LiveState::Active {
                    return Err(LiveError::State);
                }
                let bytes = STANDARD
                    .decode(v["delta"].as_str().ok_or(LiveError::Protocol)?)
                    .map_err(|_| LiveError::Protocol)?;
                if bytes.is_empty() || bytes.len() > MAX_AUDIO {
                    return Err(LiveError::Capacity);
                }
                Ok(LiveInput::Audio(bytes))
            }
            // Additional supported events are added with their consumers. Unknown
            // extensions carry no authority and must never trigger backend work.
            _ => Ok(LiveInput::Ignored),
        }
    }
}
