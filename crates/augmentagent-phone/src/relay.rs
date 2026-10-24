//! ConversationRelay protocol state. One instance belongs to one claimed
//! WebSocket. Transport-level reconnects cannot recreate that claim.
use crate::Error;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Debug)]
pub struct RelayTurn {
    pub id: String,
    pub text: String,
    generation: u64,
}
pub enum RelayInput {
    Ready,
    Ignore,
    Interrupted,
    ProviderError(u32),
    Turn(RelayTurn),
}
#[derive(Deserialize)]
#[serde(tag = "type")]
enum Incoming {
    #[serde(rename = "setup")]
    Setup {
        #[serde(rename = "accountSid")]
        account_sid: String,
        #[serde(rename = "callSid")]
        call_sid: String,
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    #[serde(rename = "prompt")]
    Prompt {
        #[serde(rename = "voicePrompt")]
        text: String,
        last: bool,
    },
    #[serde(rename = "interrupt")]
    Interrupt {
        #[serde(rename = "utteranceUntilInterrupt")]
        heard: String,
        #[serde(rename = "durationUntilInterruptMs")]
        duration: u64,
    },
    #[serde(rename = "dtmf")]
    Dtmf { digit: String },
    #[serde(rename = "error")]
    Error { code: Option<u32> },
}
struct TurnOutput {
    sent: HashMap<String, ([u8; 32], bool)>,
    final_spoken: bool,
    /// Concatenation of every streamed token so far. Deduplication covers the
    /// whole streamed answer, never an individual chunk.
    streamed: String,
}
pub struct RelaySession {
    call_id: String,
    account_sid: String,
    call_sid: String,
    ready: bool,
    sequence: u64,
    generation: u64,
    outputs: HashMap<String, TurnOutput>,
    heard_until_interrupt: Option<String>,
}
impl RelaySession {
    pub fn new(call_id: &str, account_sid: &str, call_sid: &str) -> Self {
        Self {
            call_id: call_id.into(),
            account_sid: account_sid.into(),
            call_sid: call_sid.into(),
            ready: false,
            sequence: 0,
            generation: 0,
            outputs: HashMap::new(),
            heard_until_interrupt: None,
        }
    }
    pub fn receive(&mut self, frame: &str) -> Result<RelayInput, Error> {
        if frame.len() > 32_768 {
            return Err(Error::InvalidArguments);
        }
        let message: Incoming = serde_json::from_str(frame).map_err(|_| Error::InvalidArguments)?;
        match message {
            Incoming::Setup {
                account_sid,
                call_sid,
                session_id,
            } => {
                if self.ready
                    || account_sid != self.account_sid
                    || call_sid != self.call_sid
                    || session_id.is_empty()
                    || session_id.len() > 128
                {
                    return Err(Error::Unauthorized);
                }
                self.ready = true;
                Ok(RelayInput::Ready)
            }
            _ if !self.ready => Err(Error::Unauthorized),
            Incoming::Prompt { text, last } => {
                if text.len() > 12_000 {
                    return Err(Error::InvalidArguments);
                }
                // TwiML pins partialPrompts=false. Defensive handling never
                // submits interim recognition to the agent.
                if !last || text.trim().is_empty() {
                    return Ok(RelayInput::Ignore);
                }
                if self.sequence >= 256 {
                    return Err(Error::InvalidArguments);
                }
                self.sequence += 1;
                let id = format!("phone:{}:{}", self.call_id, self.sequence);
                self.outputs.insert(
                    id.clone(),
                    TurnOutput {
                        sent: HashMap::new(),
                        final_spoken: false,
                        streamed: String::new(),
                    },
                );
                Ok(RelayInput::Turn(RelayTurn {
                    id,
                    text,
                    generation: self.generation,
                }))
            }
            Incoming::Interrupt { heard, duration } => {
                if heard.len() > 12_000 || duration > 900_000 {
                    return Err(Error::InvalidArguments);
                }
                self.generation += 1;
                self.heard_until_interrupt = Some(heard);
                Ok(RelayInput::Interrupted)
            }
            Incoming::Error { code: Some(code) } => Ok(RelayInput::ProviderError(code)),
            Incoming::Dtmf { digit } if digit.len() == 1 && "0123456789*#".contains(&digit) => {
                Ok(RelayInput::Ignore)
            }
            _ => Err(Error::InvalidArguments),
        }
    }
    /// Provider-reported heard prefix, distinct from the full generated answer.
    pub fn take_interrupted_prefix(&mut self) -> Option<String> {
        self.heard_until_interrupt.take()
    }
    /// Called for explicit user-facing speech only. Tool results and reasoning
    /// are never connected to this API. IDs are scoped by the durable turn ID.
    pub fn speech(
        &mut self,
        turn: &RelayTurn,
        utterance_id: &str,
        text: &str,
        final_output: bool,
    ) -> Result<Option<Value>, Error> {
        if turn.generation != self.generation {
            return Ok(None);
        }
        if utterance_id.is_empty() || utterance_id.len() > 64 || text.len() > 12_000 {
            return Err(Error::InvalidArguments);
        }
        let output = self.outputs.get_mut(&turn.id).ok_or(Error::Unauthorized)?;
        let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if let Some(old) = output.sent.get(utterance_id) {
            if old != &(hash, final_output) {
                return Err(Error::InvalidArguments);
            }
            return Ok(None);
        }
        if output.final_spoken {
            return Ok(None);
        }
        if output.sent.len() >= 100 {
            return Err(Error::InvalidArguments);
        }
        output
            .sent
            .insert(utterance_id.into(), (hash, final_output));
        output.final_spoken = final_output;
        // Agent-authored utterances may carry Markdown; receipts are keyed on
        // the raw text, the provider hears the sanitized form.
        Ok(Some(text_frame(
            &crate::speech::SpeechTextTransform::apply(text),
            true,
        )))
    }
    /// One streamed token with `last:false`. Tokens are forwarded untrimmed
    /// so spacing survives concatenation on the provider side. Nothing is sent
    /// for a stale generation, a closed talk cycle, or an empty token.
    pub fn speak_partial(&mut self, turn: &RelayTurn, text: &str) -> Result<Option<Value>, Error> {
        self.stream(turn, text, false)
    }
    /// Closes the talk cycle: the final token (possibly empty) carries
    /// `last:true`. Afterwards the automatic final answer and any late partial
    /// for this turn are suppressed.
    pub fn finish_stream(&mut self, turn: &RelayTurn, text: &str) -> Result<Option<Value>, Error> {
        self.stream(turn, text, true)
    }
    fn stream(&mut self, turn: &RelayTurn, text: &str, last: bool) -> Result<Option<Value>, Error> {
        if turn.generation != self.generation {
            return Ok(None);
        }
        if text.len() > 12_000 {
            return Err(Error::InvalidArguments);
        }
        let output = self.outputs.get_mut(&turn.id).ok_or(Error::Unauthorized)?;
        if output.final_spoken || (!last && text.is_empty()) {
            return Ok(None);
        }
        output.streamed.push_str(text);
        if last {
            let hash: [u8; 32] = Sha256::digest(output.streamed.as_bytes()).into();
            output.sent.insert("final".into(), (hash, true));
            output.final_spoken = true;
        }
        Ok(Some(text_frame(text, last)))
    }
    pub fn finish(&mut self, turn: &RelayTurn, text: &str) -> Result<Option<Value>, Error> {
        if text.is_empty() || self.outputs.get(&turn.id).is_some_and(|o| o.final_spoken) {
            return Ok(None);
        }
        self.speech(turn, "final", text, true)
    }
}
fn text_frame(token: &str, last: bool) -> Value {
    json!({"type":"text","token":token,"last":last,"interruptible":true,"preemptible":false})
}
