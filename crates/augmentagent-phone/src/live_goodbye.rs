//! Conservative local goodbye policy. Live transcript deltas have no completed
//! turn event, so quiet time is an application heuristic, not provider proof.
//! Actual recognition and the <=5s hangup gate still require a live canary.
use crate::{end_call::is_goodbye_phrase, live_session::LiveInput};
use std::{collections::VecDeque, time::Duration};
use tokio::time::Instant;
const QUIET: Duration = Duration::from_millis(1200);
const UTTERANCE_GAP_MS: f64 = 800.;
const MAX_TEXT: usize = 512;
#[derive(Default)]
pub struct LiveGoodbye {
    text: String,
    overflow: bool,
    last_end: Option<f64>,
    last_text: Option<Instant>,
    last_voice: Option<Instant>,
    seen: VecDeque<String>,
}
impl LiveGoodbye {
    pub fn observe(&mut self, input: &LiveInput) {
        let LiveInput::Transcript {
            event_id,
            user: true,
            text,
            start_ms,
            end_ms,
        } = input
        else {
            return;
        };
        if self.seen.contains(event_id) {
            return;
        }
        if self.seen.len() == 64 {
            self.seen.pop_front();
        }
        self.seen.push_back(event_id.clone());
        if self
            .last_end
            .is_some_and(|end| *start_ms - end >= UTTERANCE_GAP_MS)
        {
            self.text.clear();
            self.overflow = false;
        }
        self.last_end = Some(self.last_end.map_or(*end_ms, |end| end.max(*end_ms)));
        self.last_text = Some(Instant::now());
        if self.overflow || self.text.len() + text.len() > MAX_TEXT {
            self.overflow = true;
            self.text.clear();
        } else {
            self.text.push_str(text);
        }
    }
    /// G.711 mu-law energy guard, not a speech recognizer. Energetic noise may
    /// postpone a goodbye, but cannot initiate one. Silence codes decode to 0.
    pub fn audio(&mut self, audio: &[u8]) {
        if audio.is_empty() {
            return;
        }
        let energy: i64 = audio
            .iter()
            .map(|byte| {
                let code = !*byte;
                let magnitude =
                    (((i64::from(code & 0x0f) << 3) + 132) << ((code & 0x70) >> 4)) - 132;
                magnitude * magnitude
            })
            .sum();
        if energy / audio.len() as i64 >= 700 * 700 {
            self.last_voice = Some(Instant::now());
        }
    }
    pub fn ready(&self) -> bool {
        !self.overflow
            && !self.text.contains(['"', '“', '”', '?'])
            && is_goodbye_phrase(&self.text)
            && self.last_text.is_some_and(|time| time.elapsed() >= QUIET)
            && self.last_voice.is_none_or(|time| time.elapsed() >= QUIET)
    }
}
