//! Preemptive generation on interim transcripts (#1555). With
//! `AUGMENTAGENT_PHONE_PREEMPTIVE=1` the relay surfaces `prompt` frames with
//! `last:false`; the fast layer may start answering one of them before the
//! final transcript arrives. Its output is buffered here and reaches the
//! socket only if the final transcript commits it.
//!
//! Nothing in this module retains or logs speech beyond the in-flight
//! utterance.
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

/// A partial needs at least this many words to start a speculation.
pub const MIN_WORDS: usize = 4;
/// Minimum time since the previous partial that changed the text.
pub const MIN_GAP: Duration = Duration::from_millis(400);
/// The final transcript may add at most this many words to the speculated text.
pub const MAX_ADDED_WORDS: usize = 2;
/// One speculation plus at most one restart per utterance.
pub const MAX_STARTS: u8 = 2;
/// Buffered speculative tokens; the model is paused once this many wait.
const MAX_BUFFERED: usize = 256;

/// Lowercased words with surrounding punctuation trimmed, so that recognition
/// formatting ("Tomorrow?" vs "tomorrow") never decides a commit.
pub fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| c.is_ascii_punctuation() && c != '\'')
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

fn is_prefix(prefix: &[String], text: &[String]) -> bool {
    prefix.len() <= text.len() && text[..prefix.len()] == *prefix
}

/// Whether `newer` keeps every word of `speculated`, in order, as a prefix.
pub fn extends(speculated: &str, newer: &str) -> bool {
    is_prefix(&words(speculated), &words(newer))
}

/// The commit rule: the speculated text is a normalized word prefix of the
/// final text, and the final adds at most [`MAX_ADDED_WORDS`] words.
pub fn commits(speculated: &str, final_text: &str) -> bool {
    let speculated = words(speculated);
    let final_words = words(final_text);
    !speculated.is_empty()
        && is_prefix(&speculated, &final_words)
        && final_words.len() - speculated.len() <= MAX_ADDED_WORDS
}

/// What one partial asks the caller to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PartialAction {
    /// The in-flight speculation no longer matches and was dropped.
    pub cancelled: bool,
    /// Start a speculation on this text.
    pub start: Option<String>,
}

/// Start/cancel/commit decisions for one utterance. Pure: time comes from
/// the tokio clock, so paused-clock tests control it exactly.
#[derive(Default)]
pub struct SpeculationPolicy {
    last_words: Vec<String>,
    last_change: Option<Instant>,
    speculating: Option<(String, Instant)>,
    starts: u8,
    cancellations: u8,
}
impl SpeculationPolicy {
    pub fn partial(&mut self, text: &str) -> PartialAction {
        let now = Instant::now();
        let current = words(text);
        let mut action = PartialAction::default();
        if current.is_empty() {
            return action;
        }
        if let Some((speculated, _)) = &self.speculating {
            if !is_prefix(&words(speculated), &current) {
                self.speculating = None;
                self.cancellations += 1;
                action.cancelled = true;
            }
        }
        let settled = self
            .last_change
            .is_some_and(|at| now.duration_since(at) >= MIN_GAP);
        if self.speculating.is_none()
            && self.starts < MAX_STARTS
            && current.len() >= MIN_WORDS
            && settled
        {
            self.speculating = Some((text.to_owned(), now));
            self.starts += 1;
            action.start = Some(text.to_owned());
        }
        if current != self.last_words {
            self.last_words = current;
            self.last_change = Some(now);
        }
        action
    }
    /// The caller could not start the speculation it was asked to start.
    pub fn abandon(&mut self) {
        if self.speculating.take().is_some() {
            self.starts -= 1;
        }
    }
    /// The final transcript: the time saved when the speculation commits.
    /// Resets the policy for the next utterance.
    pub fn finalize(&mut self, final_text: &str) -> (bool, Option<Duration>) {
        let speculated = self.starts > 0;
        let saved = self
            .speculating
            .take()
            .filter(|(text, _)| commits(text, final_text))
            .map(|(_, at)| at.elapsed());
        *self = Self::default();
        (speculated, saved)
    }
    pub fn starts(&self) -> u8 {
        self.starts
    }
    pub fn cancellations(&self) -> u8 {
        self.cancellations
    }
}

#[derive(Default)]
struct Flags {
    wants_tool: AtomicBool,
    failed: AtomicBool,
}

/// An in-flight speculative answer. Dropping it cancels the generation;
/// buffered output is discarded with it.
pub struct Speculative {
    text: String,
    tokens: mpsc::Receiver<String>,
    flags: Arc<Flags>,
    cancel: CancellationToken,
}
impl Drop for Speculative {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
/// Write side for the generating task. Nothing written here reaches the
/// socket unless the speculation is committed.
pub struct SpeculativeSink {
    tokens: mpsc::Sender<String>,
    flags: Arc<Flags>,
}
impl SpeculativeSink {
    /// Buffer one token. `false` once the speculation was dropped.
    pub async fn token(&self, text: String) -> bool {
        self.tokens.send(text).await.is_ok()
    }
    /// The model asked for a tool: the speculation stops and the committed
    /// turn re-plans with tools enabled.
    pub fn wants_tool(&self) {
        self.flags.wants_tool.store(true, Ordering::SeqCst);
    }
    pub fn failed(&self) {
        self.flags.failed.store(true, Ordering::SeqCst);
    }
}
impl Speculative {
    /// Run `generate` in the background until it finishes or the speculation
    /// is dropped.
    pub fn spawn<F, Fut>(text: &str, generate: F) -> Self
    where
        F: FnOnce(SpeculativeSink) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, tokens) = mpsc::channel(MAX_BUFFERED);
        let flags = Arc::new(Flags::default());
        let cancel = CancellationToken::new();
        let work = generate(SpeculativeSink {
            tokens: tx,
            flags: flags.clone(),
        });
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = work => {}
            }
        });
        Self {
            text: text.to_owned(),
            tokens,
            flags,
            cancel,
        }
    }
    /// The interim transcript this speculation answers.
    pub fn text(&self) -> &str {
        &self.text
    }
    /// The generation stopped for a tool call or a model failure, so its
    /// output must not stand as the answer.
    pub fn unusable(&self) -> bool {
        self.flags.wants_tool.load(Ordering::SeqCst) || self.flags.failed.load(Ordering::SeqCst)
    }
    /// Next buffered token; `None` once the generation has ended.
    pub async fn next_token(&mut self) -> Option<String> {
        self.tokens.recv().await
    }
}

/// Final-prompt outcome for one utterance.
pub struct Finalized {
    /// A speculation was started for this utterance.
    pub speculated: bool,
    /// Final-prompt time minus speculation start, when committed.
    pub saved: Option<Duration>,
    /// The committed speculation, to be flushed by the turn.
    pub speculation: Option<Speculative>,
}

/// Per-call slot: the policy plus at most one speculation in flight.
#[derive(Default)]
pub struct Speculator {
    policy: SpeculationPolicy,
    active: Option<Speculative>,
}
impl Speculator {
    /// Route one interim transcript. `start` launches a speculation; `None`
    /// means the agent does not speculate.
    pub fn partial(&mut self, text: &str, start: impl FnOnce(&str) -> Option<Speculative>) {
        let action = self.policy.partial(text);
        if action.cancelled {
            self.active = None;
        }
        if let Some(text) = action.start {
            self.active = start(&text);
            if self.active.is_none() {
                self.policy.abandon();
            }
        }
    }
    pub fn finalize(&mut self, final_text: &str) -> Finalized {
        let (speculated, saved) = self.policy.finalize(final_text);
        let active = self.active.take();
        let speculation = saved.and(active);
        Finalized {
            speculated,
            saved: speculation.as_ref().and(saved),
            speculation,
        }
    }
    /// Drop any speculation and forget the utterance (interruption, busy
    /// worker).
    pub fn reset(&mut self) {
        self.active = None;
        self.policy = SpeculationPolicy::default();
    }
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
    pub fn starts(&self) -> u8 {
        self.policy.starts()
    }
    pub fn cancellations(&self) -> u8 {
        self.policy.cancellations()
    }
}
