//! Primary Live WebSocket lifecycle. This module never owns dialing authority,
//! credentials or endpoints. The runtime must enforce `deadline()` even when
//! the peer sends no frames, and close its socket on any terminal state.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};
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
    CommentaryAccepted {
        client_event_id: String,
    },
    /// Confirmed nonfatal rejection of a locally tracked commentary command.
    CommentaryRejected {
        client_event_id: String,
    },
    Started,
    Audio(Vec<u8>),
    Transcript {
        event_id: String,
        user: bool,
        text: String,
        start_ms: f64,
        end_ms: f64,
    },
    Delegation {
        id: String,
        offset_ms: f64,
    },
    Closed {
        reason: CloseReason,
        seconds: Option<f64>,
    },
}
#[derive(Clone, Copy)]
struct CommentaryCommand {
    delegated: bool,
    bytes: usize,
    created: Instant,
}
pub struct LiveSession {
    state: LiveState,
    instructions: String,
    greet: bool,
    voice: crate::live_voice::LiveVoice,
    id: Option<String>,
    deadline: Option<Instant>,
    delegations: HashMap<String, f64>,
    commentary_commands: HashMap<String, CommentaryCommand>,
}
impl LiveSession {
    pub fn new(instructions: &str) -> Result<Self, LiveError> {
        Self::with_voice(instructions, crate::live_voice::LiveVoice::default(), false)
    }
    pub fn with_voice(
        instructions: &str,
        voice: crate::live_voice::LiveVoice,
        greet: bool,
    ) -> Result<Self, LiveError> {
        if instructions.is_empty() {
            return Err(LiveError::Configuration);
        }
        let instructions = voice.instructions(instructions);
        if instructions.len() > 16_000 {
            return Err(LiveError::Configuration);
        }
        Ok(Self {
            state: LiveState::Created,
            instructions,
            greet,
            voice,
            id: None,
            deadline: None,
            delegations: HashMap::new(),
            commentary_commands: HashMap::new(),
        })
    }
    /// Answered owner calls get a startup developer message. Readiness probes
    /// use new() so they never request a spoken greeting.
    pub fn for_call(instructions: &str) -> Result<Self, LiveError> {
        let mut session = Self::new(instructions)?;
        session.greet = true;
        Ok(session)
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
        let mut event = json!({"type":"session.start","session":{
            "model":MODEL,"instructions":self.instructions,
            "audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":self.voice.wire()}},
            "delegation":{"type":"client"}}});
        if self.greet {
            event["session"]["input"] = json!([{"role":"developer","content":[{
                "type":"input_text","text":"Greet the caller now in English with one brief friendly sentence, then ask what they need. Use their saved first name only if it is present in the approved profile context. Do not recite profile details or wait for the caller to speak first."
            }]}]);
        }
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
    /// Cap UTF-8 bytes conservatively: byte-level tokenization cannot exceed
    /// this 500-byte bound. Larger results arrive as ordered, acknowledged
    /// pieces; no extra model call or silent truncation is required.
    pub fn commentary(&mut self, delegation_id: &str, content: &str) -> Result<Value, LiveError> {
        if !self.delegations.contains_key(delegation_id) {
            return Err(LiveError::Protocol);
        }
        self.commentary_context(Some(delegation_id), content)
    }
    /// Nullable delegation IDs are the documented general session context path.
    /// Only a definitive rejected reference may move an existing task here.
    pub(crate) fn commentary_context(
        &mut self,
        delegation_id: Option<&str>,
        content: &str,
    ) -> Result<Value, LiveError> {
        self.check_deadline()?;
        if self.state != LiveState::Active {
            return Err(LiveError::State);
        }
        if content.trim().is_empty() || content.len() > 500 {
            return Err(LiveError::Capacity);
        }
        if self.commentary_commands.len() >= 128 {
            return Err(LiveError::Capacity);
        }
        let event_id = uuid::Uuid::new_v4().to_string();
        self.commentary_commands.insert(
            event_id.clone(),
            CommentaryCommand {
                delegated: delegation_id.is_some(),
                bytes: content.len(),
                created: Instant::now(),
            },
        );
        Ok(
            json!({"type":"session.commentary.append", "event_id":event_id,
            "delegation_id":delegation_id,"content":content}),
        )
    }
    /// A missing acknowledgment is ambiguous. Forget correlation when the
    /// bounded coordinator abandons it; never turn a later error into repair.
    pub(crate) fn forget_commentary(&mut self, event_id: &str) {
        self.commentary_commands.remove(event_id);
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
                let correlated = error["client_event_id"].as_str().and_then(|id| {
                    self.commentary_commands
                        .get(id)
                        .map(|command| (id.to_owned(), *command))
                });
                // This exact nonfatal envelope and nullable-ID repair were
                // verified against the real Live provider with synthetic data.
                // Do not generalize invalid_request_error into blanket recovery.
                let lost_reference = self.state == LiveState::Active
                    && error["type"] == "invalid_request_error"
                    && error["param"] == "delegation_id"
                    && error["code"].is_null()
                    && error["message"] == "Unknown client or Decision delegation.";
                let safe_param = match error["param"].as_str() {
                    Some("delegation_id") => "delegation_id",
                    Some("content") => "content",
                    Some("audio") => "audio",
                    _ => "other_or_absent",
                };
                // Only locally generated IDs and allowlisted labels escape.
                // Never log provider message/param/code or unknown peer IDs.
                tracing::warn!(
                    event_id = correlated
                        .as_ref()
                        .map(|(id, _)| id.as_str())
                        .unwrap_or("uncorrelated"),
                    command = if correlated.is_some() {
                        "commentary"
                    } else {
                        "unknown"
                    },
                    param = safe_param,
                    context_bytes = correlated.as_ref().map(|(_, command)| command.bytes),
                    command_age_ms =
                        correlated
                            .as_ref()
                            .map(|(_, command)| command.created.elapsed().as_millis() as u64),
                    session_continues = lost_reference,
                    repair_allowed = lost_reference
                        && correlated
                            .as_ref()
                            .is_some_and(|(_, command)| command.delegated),
                    "phone Live command rejected"
                );
                if lost_reference {
                    if let Some((id, command)) = correlated {
                        self.commentary_commands.remove(&id);
                        if command.delegated {
                            return Ok(LiveInput::CommentaryRejected {
                                client_event_id: id,
                            });
                        }
                    }
                    // The probed error is nonfatal even when its command can
                    // no longer be correlated. Never speculate about a resend.
                    return Ok(LiveInput::Ignored);
                }
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
                    || s["audio"]["output"]["voice"] != self.voice.wire()
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
            "session.commentary.appended" => {
                if self.state == LiveState::Closing {
                    return Ok(LiveInput::Ignored);
                }
                if self.state != LiveState::Active {
                    return Err(LiveError::State);
                }
                let Some(client_id) = v.get("client_event_id") else {
                    return Ok(LiveInput::Ignored);
                };
                let client_event_id = bounded_id(client_id)?;
                self.commentary_commands.remove(&client_event_id);
                Ok(LiveInput::CommentaryAccepted { client_event_id })
            }
            "session.input_transcript.delta" | "session.output_transcript.delta" => {
                if self.state == LiveState::Closing {
                    return Ok(LiveInput::Ignored);
                }
                if self.state != LiveState::Active {
                    return Err(LiveError::State);
                }
                let event_id = bounded_id(&v["event_id"])?;
                let start_ms = timeline(&v["start_ms"])?;
                let end_ms = timeline(&v["end_ms"])?;
                if end_ms < start_ms {
                    return Err(LiveError::Protocol);
                }
                let text = v["delta"].as_str().ok_or(LiveError::Protocol)?;
                if text.len() > 8000 {
                    return Err(LiveError::Capacity);
                }
                Ok(LiveInput::Transcript {
                    event_id,
                    user: kind == "session.input_transcript.delta",
                    text: text.into(),
                    start_ms,
                    end_ms,
                })
            }
            "session.delegation.created" => {
                if self.state == LiveState::Closing {
                    return Ok(LiveInput::Ignored);
                }
                if self.state != LiveState::Active {
                    return Err(LiveError::State);
                }
                bounded_id(&v["event_id"])?;
                let delegation = &v["delegation"];
                if delegation["type"] != "delegation" || delegation["target"] != "client" {
                    return Err(LiveError::Protocol);
                }
                let id = bounded_id(&delegation["id"])?;
                let offset_ms = timeline(&v["offset_ms"])?;
                if let Some(previous) = self.delegations.get(&id) {
                    return if *previous == offset_ms {
                        Ok(LiveInput::Ignored)
                    } else {
                        Err(LiveError::Protocol)
                    };
                }
                if self.delegations.len() >= 128 {
                    return Err(LiveError::Capacity);
                }
                self.delegations.insert(id.clone(), offset_ms);
                Ok(LiveInput::Delegation { id, offset_ms })
            }
            // Additional supported events are added with their consumers. Unknown
            // extensions carry no authority and must never trigger backend work.
            _ => Ok(LiveInput::Ignored),
        }
    }
}

fn bounded_id(value: &Value) -> Result<String, LiveError> {
    value
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(str::to_owned)
        .ok_or(LiveError::Protocol)
}
fn timeline(value: &Value) -> Result<f64, LiveError> {
    value
        .as_f64()
        .filter(|ms| ms.is_finite() && *ms >= 0.0 && *ms <= 86_400_000.0)
        .ok_or(LiveError::Protocol)
}
