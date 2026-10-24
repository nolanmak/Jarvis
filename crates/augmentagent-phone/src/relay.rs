//! ConversationRelay protocol state. One instance belongs to one claimed
//! WebSocket. Transport-level reconnects cannot recreate that claim.
use crate::Error;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

#[derive(Debug)]
pub struct RelayTurn {
    pub id: String,
    pub text: String,
    generation: u64,
}
pub enum RelayInput {
    Ready,
    Ignore,
    /// A confirmed interruption with no new turn: the heard prefix is ready in
    /// [`RelaySession::take_interrupted_prefix`].
    Interrupted,
    /// Playback stopped; whether it was a real interruption is decided by the
    /// next prompt or by the false-interruption timeout.
    InterruptPending,
    /// A false interruption: the interrupted answer resumes from the unspoken
    /// remainder (`None` when nothing is left to say).
    Resumed(Option<Resume>),
    ProviderError(u32),
    /// Provider `error` frame received after setup and before the first
    /// prompt whose description names the provider, model or voice: the
    /// configured STT/TTS selection was rejected. The description is dropped.
    SetupProviderRejected,
    Turn(RelayTurn),
}
/// Manual Debug: caller speech and provider descriptions never appear.
impl std::fmt::Debug for RelayInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => f.write_str("Ready"),
            Self::Ignore => f.write_str("Ignore"),
            Self::Interrupted => f.write_str("Interrupted"),
            Self::InterruptPending => f.write_str("InterruptPending"),
            Self::Resumed(resume) => write!(
                f,
                "Resumed({})",
                resume.as_ref().map_or("none", |r| r.turn_id.as_str())
            ),
            Self::ProviderError(code) => write!(f, "ProviderError({code})"),
            Self::SetupProviderRejected => f.write_str("SetupProviderRejected"),
            Self::Turn(turn) => write!(f, "Turn({})", turn.id),
        }
    }
}
/// Whether a provider error description points at the configured speech
/// providers rather than at a malformed message. Case-insensitive keyword
/// match; the description itself is never retained.
pub fn names_voice_setup(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    [
        "voice",
        "ttsprovider",
        "transcriptionprovider",
        "speechmodel",
        "provider",
        "model",
        "tts",
        "transcription",
        "elevenlabs",
        "deepgram",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}
/// The resumed remainder of an interrupted answer, ready to write.
#[derive(Debug)]
pub struct Resume {
    pub turn_id: String,
    pub frame: Value,
}
/// Barge-in handling (#1550). Short backchannel speech while Jarvis is talking
/// does not cancel the answer.
#[derive(Clone, Debug)]
pub struct BargeIn {
    min_words: usize,
    backchannels: Vec<String>,
    false_interrupt: Duration,
    resume: bool,
}
/// Default backchannel phrases, already normalized (see [`normalize`]).
pub const DEFAULT_BACKCHANNELS: &[&str] = &[
    "ok", "okay", "yeah", "yes", "mm", "mhm", "mm hm", "mm hmm", "uh huh", "right", "sure",
    "got it",
];
impl Default for BargeIn {
    fn default() -> Self {
        Self {
            min_words: 3,
            backchannels: DEFAULT_BACKCHANNELS.iter().map(|s| (*s).into()).collect(),
            false_interrupt: Duration::from_millis(2_000),
            resume: true,
        }
    }
}
impl BargeIn {
    /// `min_words` 1–10 (1 disables the filter: every interrupt invalidates at
    /// once); 1–64 phrases, normalized, each at most 64 bytes; timeout
    /// 500–5,000 ms. Errors name the operator variable, never the value.
    pub fn new(
        min_words: usize,
        backchannels: Vec<String>,
        false_interrupt_ms: u64,
        resume: bool,
    ) -> Result<Self, Error> {
        use crate::config::{BACKCHANNEL_PHRASES, BARGE_MIN_WORDS, FALSE_INTERRUPT_MS};
        if !(1..=10).contains(&min_words) {
            return Err(Error::InvalidSetting(BARGE_MIN_WORDS));
        }
        let backchannels: Vec<String> = backchannels
            .iter()
            .map(|b| normalize(b))
            .filter(|b| !b.is_empty())
            .collect();
        if backchannels.is_empty()
            || backchannels.len() > 64
            || backchannels.iter().any(|b| b.len() > 64)
        {
            return Err(Error::InvalidSetting(BACKCHANNEL_PHRASES));
        }
        if !(500..=5_000).contains(&false_interrupt_ms) {
            return Err(Error::InvalidSetting(FALSE_INTERRUPT_MS));
        }
        Ok(Self {
            min_words,
            backchannels,
            false_interrupt: Duration::from_millis(false_interrupt_ms),
            resume,
        })
    }
    pub fn min_words(&self) -> usize {
        self.min_words
    }
    pub fn backchannels(&self) -> &[String] {
        &self.backchannels
    }
    pub fn false_interrupt(&self) -> Duration {
        self.false_interrupt
    }
    pub fn resume(&self) -> bool {
        self.resume
    }
    /// Speech shorter than `min_words` that matches a backchannel phrase is
    /// not an interruption. A goodbye phrase (#1557) never is, so the owner
    /// can always end the call over Jarvis.
    fn is_backchannel(&self, text: &str) -> bool {
        let normalized = normalize(text);
        normalized.split(' ').count() < self.min_words
            && self.backchannels.contains(&normalized)
            && !crate::end_call::is_goodbye_phrase(text)
    }
}
/// Lowercase, hyphens and punctuation to spaces, whitespace collapsed:
/// "Mm-hmm." becomes "mm hmm".
pub fn normalize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
/// The part of `spoken` the caller has not heard. `cycle_starts` are the byte
/// offsets where each talk cycle began; the heard prefix is matched against the
/// latest cycle first. A cut inside a word restarts that word. Without an
/// exact prefix, the remainder starts at the next sentence boundary after the
/// position the spoken duration implies within the latest cycle.
fn remainder<'a>(
    spoken: &'a str,
    cycle_starts: &[usize],
    heard: &str,
    duration_ms: u64,
) -> &'a str {
    for &start in cycle_starts.iter().rev() {
        if spoken[start..].starts_with(heard) {
            let mut cut = start + heard.len();
            let word = |i: usize| {
                spoken[i..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
            };
            if cut < spoken.len() && cut > start && word(cut) {
                while cut > start {
                    let prev = spoken[..cut]
                        .char_indices()
                        .next_back()
                        .map_or(start, |(i, _)| i);
                    if !word(prev) {
                        break;
                    }
                    cut = prev;
                }
            }
            return spoken[cut..].trim_start();
        }
    }
    let start = cycle_starts.last().copied().unwrap_or(0);
    let estimate = usize::try_from(duration_ms.saturating_mul(SPOKEN_CHARS_PER_SECOND) / 1000)
        .unwrap_or(usize::MAX);
    let mut position = start.saturating_add(estimate).min(spoken.len());
    while !spoken.is_char_boundary(position) {
        position += 1;
    }
    let rest = &spoken[position..];
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?')
            && chars.peek().is_some_and(|(_, next)| next.is_whitespace())
        {
            return rest[i + 1..].trim_start();
        }
    }
    ""
}
/// Nominal TTS speaking rate used to place a cut when the heard text does not
/// match what was sent.
const SPOKEN_CHARS_PER_SECOND: u64 = 15;
struct PendingInterrupt {
    turn_id: String,
    heard: String,
    duration_ms: u64,
    deadline: Instant,
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
    Error {
        code: Option<u32>,
        description: Option<String>,
    },
    /// Relay debug/event traffic (`debug`/`events` TwiML attributes). Only the
    /// event name is read; values are never logged or stored.
    #[serde(rename = "info")]
    Info {
        name: Option<String>,
        last: Option<bool>,
    },
    #[serde(other)]
    Unknown,
}
struct TurnOutput {
    sent: HashMap<String, ([u8; 32], bool)>,
    final_spoken: bool,
    /// Concatenation of every streamed token so far. Deduplication covers the
    /// whole streamed answer, never an individual chunk.
    streamed: String,
    /// Every token written for this turn, utterances included, in order.
    spoken: String,
    cycle_starts: Vec<usize>,
    /// The latest frame had `last:false`.
    cycle_open: bool,
    generation: u64,
}
impl TurnOutput {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            sent: HashMap::new(),
            final_spoken: false,
            streamed: String::new(),
            spoken: String::new(),
            cycle_starts: Vec::new(),
            cycle_open: false,
        }
    }
    fn record(&mut self, token: &str, last: bool) {
        if !self.cycle_open {
            self.cycle_starts.push(self.spoken.len());
        }
        self.spoken.push_str(token);
        self.cycle_open = !last;
    }
}
/// Twilio closes a ConversationRelay socket after ten consecutive malformed
/// messages; incoming frames are held to the same bound (#1549).
pub const INVALID_FRAME_LIMIT: u32 = 10;
pub struct RelaySession {
    call_id: String,
    account_sid: String,
    call_sid: String,
    ready: bool,
    prompted: bool,
    sequence: u64,
    generation: u64,
    outputs: HashMap<String, TurnOutput>,
    heard_until_interrupt: Option<String>,
    consecutive_invalid: u32,
    debug_events: bool,
    invalid_frames: u64,
    played_event: Option<tokio::time::Instant>,
    owner_activity: bool,
    barge: BargeIn,
    /// Turn that most recently wrote speech: what an `interrupt` cut off.
    speaking: Option<String>,
    pending: Option<PendingInterrupt>,
    /// A tokens-played event marked `last:true` arrived (#1557).
    played_final: bool,
    ended: bool,
}
impl RelaySession {
    pub fn new(call_id: &str, account_sid: &str, call_sid: &str) -> Self {
        Self {
            call_id: call_id.into(),
            account_sid: account_sid.into(),
            call_sid: call_sid.into(),
            ready: false,
            prompted: false,
            sequence: 0,
            generation: 0,
            outputs: HashMap::new(),
            heard_until_interrupt: None,
            consecutive_invalid: 0,
            debug_events: false,
            invalid_frames: 0,
            played_event: None,
            owner_activity: false,
            barge: BargeIn::default(),
            speaking: None,
            pending: None,
            played_final: false,
            ended: false,
        }
    }
    /// With Relay debug events requested in TwiML, unknown frame types are
    /// categorized and ignored instead of ending the session.
    pub fn with_debug_events(mut self, enabled: bool) -> Self {
        self.debug_events = enabled;
        self
    }
    pub fn with_barge_in(mut self, barge: BargeIn) -> Self {
        self.barge = barge;
        self
    }
    /// Output epoch; speech for an older generation is never sent.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// When a pending interruption is treated as false if no prompt arrives.
    pub fn false_interrupt_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|p| p.deadline)
    }
    /// After the deadline, resolve the pending interruption as false.
    pub fn poll_false_interrupt(&mut self) -> Option<RelayInput> {
        if Instant::now() < self.false_interrupt_deadline()? {
            return None;
        }
        Some(self.false_interruption())
    }
    /// Resume the cut answer, or with resume off drop its remainder: the turn
    /// is closed without a new generation and reported as interrupted.
    fn false_interruption(&mut self) -> RelayInput {
        if self.barge.resume {
            return RelayInput::Resumed(self.resume());
        }
        let Some(pending) = self.pending.take() else {
            return RelayInput::Ignore;
        };
        if let Some(output) = self.outputs.get_mut(&pending.turn_id) {
            output.final_spoken = true;
            output.cycle_open = false;
        }
        self.speaking = None;
        self.heard_until_interrupt = Some(pending.heard);
        RelayInput::Interrupted
    }
    /// Frames rejected as malformed, unauthorized or unsupported.
    pub fn invalid_frames(&self) -> u64 {
        self.invalid_frames
    }
    /// Whether the owner spoke (any `prompt`, partial or final, or an
    /// `interrupt`) since the last call. Restarts the silence clock (#1556).
    pub fn take_owner_activity(&mut self) -> bool {
        std::mem::take(&mut self.owner_activity)
    }
    /// Receipt time of the latest tokens-played debug event, consumed once.
    pub fn take_played_event(&mut self) -> Option<tokio::time::Instant> {
        self.played_event.take()
    }
    /// Whether a tokens-played event closing a talk cycle (`last:true`)
    /// arrived since the last call. Consumed once.
    pub fn take_played_final(&mut self) -> bool {
        std::mem::take(&mut self.played_final)
    }
    /// Any accepted frame resets the consecutive invalid run; a rejected
    /// frame extends it. Control frames are not messages and change nothing.
    pub fn receive(&mut self, frame: &str) -> Result<RelayInput, Error> {
        let result = self.receive_frame(frame);
        match result {
            Ok(_) => self.consecutive_invalid = 0,
            Err(_) => {
                self.invalid_frames += 1;
                self.consecutive_invalid = self.consecutive_invalid.saturating_add(1);
            }
        }
        result
    }
    /// A binary or otherwise unsupported frame counts like a malformed message.
    pub fn note_unsupported_frame(&mut self) -> u32 {
        self.invalid_frames += 1;
        self.consecutive_invalid = self.consecutive_invalid.saturating_add(1);
        self.consecutive_invalid
    }
    pub fn consecutive_invalid_frames(&self) -> u32 {
        self.consecutive_invalid
    }
    /// Protocol-level `end` without handoff data, sent when the invalid-frame
    /// limit is reached. The agent's goodbye path uses `end(reason)`.
    pub fn end_frame(&self) -> Value {
        json!({"type":"end"})
    }
    fn receive_frame(&mut self, frame: &str) -> Result<RelayInput, Error> {
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
                self.owner_activity = true;
                // TwiML pins partialPrompts=false. Defensive handling never
                // submits interim recognition to the agent.
                if !last || text.trim().is_empty() {
                    return Ok(RelayInput::Ignore);
                }
                if self.pending.is_some() && self.barge.is_backchannel(&text) {
                    return Ok(self.false_interruption());
                }
                if let Some(pending) = self.pending.take() {
                    self.confirm_interrupt(pending.heard);
                } else if !self
                    .outputs
                    .values()
                    .any(|o| o.generation == self.generation && !o.final_spoken)
                {
                    // Jarvis is silent with no answer in progress: the prompt
                    // opens a new generation. An answer still being worked on
                    // is not invalidated by a follow-up prompt.
                    self.advance_generation();
                }
                if self.sequence >= 256 {
                    return Err(Error::InvalidArguments);
                }
                self.prompted = true;
                self.sequence += 1;
                let id = format!("phone:{}:{}", self.call_id, self.sequence);
                self.outputs
                    .insert(id.clone(), TurnOutput::new(self.generation));
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
                self.owner_activity = true;
                // Min words 1 leaves no speech short enough to be a backchannel.
                if let Some(turn_id) = self.speaking.clone().filter(|_| self.barge.min_words > 1) {
                    self.pending = Some(PendingInterrupt {
                        turn_id,
                        heard,
                        duration_ms: duration,
                        deadline: Instant::now() + self.barge.false_interrupt,
                    });
                    return Ok(RelayInput::InterruptPending);
                }
                self.confirm_interrupt(heard);
                Ok(RelayInput::Interrupted)
            }
            Incoming::Error { description, .. }
                if !self.prompted && description.as_deref().is_some_and(names_voice_setup) =>
            {
                Ok(RelayInput::SetupProviderRejected)
            }
            Incoming::Error {
                code: Some(code), ..
            } => Ok(RelayInput::ProviderError(code)),
            Incoming::Info { name, last } => {
                // Without debug events requested, nothing is attributed.
                if self.debug_events && name.as_deref() == Some("tokensPlayed") {
                    self.played_event = Some(tokio::time::Instant::now());
                    self.played_final |= last == Some(true);
                }
                Ok(RelayInput::Ignore)
            }
            Incoming::Unknown if self.debug_events => Ok(RelayInput::Ignore),
            Incoming::Dtmf { digit } if digit.len() == 1 && "0123456789*#".contains(&digit) => {
                Ok(RelayInput::Ignore)
            }
            _ => Err(Error::InvalidArguments),
        }
    }
    /// A real interruption: stale output is dropped from now on.
    fn confirm_interrupt(&mut self, heard: String) {
        self.advance_generation();
        self.heard_until_interrupt = Some(heard);
    }
    fn advance_generation(&mut self) {
        self.generation += 1;
        self.speaking = None;
    }
    /// Resume the pending interrupted turn: one frame with the unspoken
    /// remainder, which starts a new talk cycle.
    fn resume(&mut self) -> Option<Resume> {
        let pending = self.pending.take()?;
        let output = self.outputs.get_mut(&pending.turn_id)?;
        let rest = remainder(
            &output.spoken,
            &output.cycle_starts,
            &pending.heard,
            pending.duration_ms,
        )
        .to_owned();
        if rest.is_empty() {
            return None;
        }
        let last = !output.cycle_open;
        output.cycle_open = false;
        output.record(&rest, last);
        Some(Resume {
            turn_id: pending.turn_id,
            frame: text_frame(&rest, last),
        })
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
        let token = crate::speech::SpeechTextTransform::apply(text);
        output.record(&token, true);
        self.speaking = Some(turn.id.clone());
        Ok(Some(text_frame(&token, true)))
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
        output.record(text, last);
        self.speaking = Some(turn.id.clone());
        Ok(Some(text_frame(text, last)))
    }
    /// One complete fast-layer announcement outside any owner turn (#1546).
    /// It opens and closes its own talk cycle with `last:true`.
    pub fn system_speech(&mut self, text: &str) -> Result<Option<Value>, Error> {
        if text.len() > 12_000 {
            return Err(Error::InvalidArguments);
        }
        let spoken = crate::speech::SpeechTextTransform::apply(text);
        if spoken.trim().is_empty() {
            return Ok(None);
        }
        // An announcement is not an answer: an interrupt over it has nothing
        // to resume (#1550).
        self.speaking = None;
        Ok(Some(text_frame(&spoken, true)))
    }
    /// Preemptible thinking sound for a turn still waiting on a tool. Nothing
    /// is sent for a stale generation or a turn whose answer has closed.
    pub fn play(&mut self, turn: &RelayTurn, source: &str) -> Result<Option<Value>, Error> {
        if turn.generation != self.generation {
            return Ok(None);
        }
        if source.len() > 2_048 {
            return Err(Error::InvalidArguments);
        }
        let output = self.outputs.get(&turn.id).ok_or(Error::Unauthorized)?;
        if output.final_spoken {
            return Ok(None);
        }
        Ok(Some(
            json!({"type":"play","source":source,"loop":1,"preemptible":true}),
        ))
    }
    /// The agent's goodbye (#1557): one complete `text` frame with
    /// `last:true` that closes the turn's talk cycle, sent even after an
    /// interruption so the owner hears why the line closes.
    pub fn goodbye(&mut self, turn: &RelayTurn, text: &str) -> Result<Value, Error> {
        if text.len() > 12_000 {
            return Err(Error::InvalidArguments);
        }
        if let Some(output) = self.outputs.get_mut(&turn.id) {
            output.final_spoken = true;
            output.streamed.push_str(text);
        }
        // After the goodbye nothing resumes (#1550): an interrupt over it is
        // not held for a backchannel decision.
        self.speaking = None;
        self.pending = None;
        let spoken = crate::speech::SpeechTextTransform::apply(text);
        let spoken = if spoken.trim().is_empty() {
            crate::end_call::DEFAULT_GOODBYE.to_owned()
        } else {
            spoken
        };
        Ok(text_frame(&spoken, true))
    }
    /// The single `end` message (#1557). `handoffData` is a JSON string the
    /// relay-ended callback returns; `None` once `end` was already produced.
    pub fn end(&mut self, reason: crate::end_call::EndCallReason) -> Option<Value> {
        if std::mem::replace(&mut self.ended, true) {
            return None;
        }
        Some(json!({
            "type": "end",
            "handoffData": crate::end_call::handoff_data(reason, &self.call_id),
        }))
    }
    pub fn finish(&mut self, turn: &RelayTurn, text: &str) -> Result<Option<Value>, Error> {
        if text.is_empty() || self.outputs.get(&turn.id).is_some_and(|o| o.final_spoken) {
            return Ok(None);
        }
        self.speech(turn, "final", text, true)
    }
}
impl RelaySession {
    /// Silence check-in (#1556): its own talk cycle; the owner may talk over
    /// it and any later speech replaces it.
    pub fn check_in(&mut self, text: &str) -> Value {
        // What plays now is not an answer: an interrupt over it has nothing
        // to resume (#1550).
        self.speaking = None;
        json!({"type":"text","token":text,"last":true,"interruptible":true,"preemptible":true})
    }
    /// Fixed silence goodbye spoken just before `end`; nothing may cut it short.
    pub fn silence_goodbye(&mut self, text: &str) -> Value {
        self.speaking = None;
        json!({"type":"text","token":text,"last":true,"interruptible":false,"preemptible":false})
    }
    /// `end` for a silence hangup (#1556), at most once per session, with the
    /// same `{reason, call_id}` handoff as an agent-initiated end (#1557).
    pub fn end_for_silence(&mut self) -> Option<Value> {
        if std::mem::replace(&mut self.ended, true) {
            return None;
        }
        Some(json!({
            "type": "end",
            "handoffData": json!({"reason": crate::silence::END_REASON, "call_id": self.call_id}).to_string(),
        }))
    }
}
fn text_frame(token: &str, last: bool) -> Value {
    json!({"type":"text","token":token,"last":last,"interruptible":true,"preemptible":false})
}
