//! Bounded Twilio Media Streams framing. Authentication and durable call claims
//! must happen at ingress; wire identities alone are not authorization.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::collections::VecDeque;
use thiserror::Error;

pub const MAX_PENDING_BYTES: usize = 16_000;
const MAX_MARKS: usize = 256;
const MAX_FRAME: usize = 32_768;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StreamError {
    #[error("invalid media stream state")]
    State,
    #[error("media stream identity mismatch")]
    Identity,
    #[error("unsupported media codec")]
    Codec,
    #[error("malformed media event")]
    Malformed,
    #[error("media event out of order")]
    Order,
    #[error("media stream capacity exceeded")]
    Capacity,
}
// Deliberately no Debug: audio must not appear in diagnostics.
pub enum StreamInput {
    Ignored,
    Started,
    Audio(Vec<u8>),
    Played { bytes: usize },
    Cleared,
    Stopped,
}
struct Mark {
    name: String,
    bytes: usize,
    cleared: bool,
}
pub struct MediaStream {
    account: String,
    call: String,
    stream: Option<String>,
    connected: bool,
    stopped: bool,
    sequence: u64,
    chunk: u64,
    timestamp: u64,
    next_mark: u64,
    pending: usize,
    marks: VecDeque<Mark>,
}
fn number(value: &Value) -> Result<u64, StreamError> {
    let text = value.as_str().ok_or(StreamError::Malformed)?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StreamError::Malformed);
    }
    text.parse().map_err(|_| StreamError::Malformed)
}
impl MediaStream {
    pub fn new(account: &str, call: &str) -> Self {
        Self {
            account: account.into(),
            call: call.into(),
            stream: None,
            connected: false,
            stopped: false,
            sequence: 0,
            chunk: 0,
            timestamp: 0,
            next_mark: 0,
            pending: 0,
            marks: VecDeque::new(),
        }
    }
    /// A protocol failure is terminal; callers must close the socket.
    pub fn receive(&mut self, text: &str) -> Result<StreamInput, StreamError> {
        let result = self.receive_inner(text);
        if result.is_err() {
            self.stopped = true;
        }
        result
    }
    fn receive_inner(&mut self, text: &str) -> Result<StreamInput, StreamError> {
        if self.stopped {
            return Err(StreamError::State);
        }
        if text.len() > MAX_FRAME {
            return Err(StreamError::Capacity);
        }
        let v: Value = serde_json::from_str(text).map_err(|_| StreamError::Malformed)?;
        let event = v["event"].as_str().ok_or(StreamError::Malformed)?;
        if event == "connected" {
            if self.connected || v["protocol"] != "Call" || v["version"] != "1.0.0" {
                return Err(StreamError::State);
            }
            self.connected = true;
            return Ok(StreamInput::Ignored);
        }
        if !self.connected {
            return Err(StreamError::State);
        }
        let sequence = number(&v["sequenceNumber"])?;
        if self.sequence.checked_add(1) != Some(sequence) {
            return Err(StreamError::Order);
        }
        if event == "start" {
            if self.stream.is_some() {
                return Err(StreamError::State);
            }
            let start = &v["start"];
            let sid = v["streamSid"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .ok_or(StreamError::Identity)?;
            if start["accountSid"] != self.account
                || start["callSid"] != self.call
                || start["streamSid"] != sid
            {
                return Err(StreamError::Identity);
            }
            if start["mediaFormat"]["encoding"] != "audio/x-mulaw"
                || start["mediaFormat"]["sampleRate"] != 8000
                || start["mediaFormat"]["channels"] != 1
                || start["tracks"] != json!(["inbound"])
            {
                return Err(StreamError::Codec);
            }
            self.stream = Some(sid.into());
            self.sequence = sequence;
            return Ok(StreamInput::Started);
        }
        let sid = self.stream.as_deref().ok_or(StreamError::State)?;
        if v["streamSid"] != sid {
            return Err(StreamError::Identity);
        }
        let input = match event {
            "media" => {
                let media = &v["media"];
                let chunk = number(&media["chunk"])?;
                let timestamp = number(&media["timestamp"])?;
                if self.chunk.checked_add(1) != Some(chunk) || timestamp < self.timestamp {
                    return Err(StreamError::Order);
                }
                if media["track"] != "inbound" {
                    return Err(StreamError::Malformed);
                }
                let bytes = STANDARD
                    .decode(media["payload"].as_str().ok_or(StreamError::Malformed)?)
                    .map_err(|_| StreamError::Malformed)?;
                if bytes.is_empty() || bytes.len() > MAX_PENDING_BYTES {
                    return Err(StreamError::Capacity);
                }
                self.chunk = chunk;
                self.timestamp = timestamp;
                StreamInput::Audio(bytes)
            }
            "mark" => {
                let mark = self.marks.front().ok_or(StreamError::Order)?;
                if v["mark"]["name"] != mark.name {
                    return Err(StreamError::Order);
                }
                let mark = self.marks.pop_front().ok_or(StreamError::Order)?;
                if mark.cleared {
                    StreamInput::Cleared
                } else {
                    self.pending -= mark.bytes;
                    StreamInput::Played { bytes: mark.bytes }
                }
            }
            "dtmf" => StreamInput::Ignored,
            "stop" => {
                if v["stop"]["accountSid"] != self.account || v["stop"]["callSid"] != self.call {
                    return Err(StreamError::Identity);
                }
                self.stopped = true;
                self.marks.clear();
                self.pending = 0;
                StreamInput::Stopped
            }
            _ => return Err(StreamError::Malformed),
        };
        self.sequence = sequence;
        Ok(input)
    }
    fn active(&self) -> Result<&str, StreamError> {
        if self.stopped {
            return Err(StreamError::State);
        }
        self.stream.as_deref().ok_or(StreamError::State)
    }
    pub fn output(&mut self, audio: &[u8]) -> Result<Vec<Value>, StreamError> {
        self.active()?;
        if audio.is_empty()
            || audio.len() > MAX_PENDING_BYTES - self.pending
            || self.marks.len() >= MAX_MARKS
        {
            return Err(StreamError::Capacity);
        }
        self.next_mark = self.next_mark.checked_add(1).ok_or(StreamError::Capacity)?;
        let name = format!("live-{}", self.next_mark);
        let frames = vec![
            json!({"event":"media","streamSid":self.stream,"media":{"payload":STANDARD.encode(audio)}}),
            json!({"event":"mark","streamSid":self.stream,"mark":{"name":name}}),
        ];
        self.pending += audio.len();
        self.marks.push_back(Mark {
            name,
            bytes: audio.len(),
            cleared: false,
        });
        Ok(frames)
    }
    pub fn clear(&mut self) -> Result<Value, StreamError> {
        self.active()?;
        for mark in &mut self.marks {
            mark.cleared = true;
        }
        self.pending = 0;
        Ok(json!({"event":"clear","streamSid":self.stream}))
    }
}
