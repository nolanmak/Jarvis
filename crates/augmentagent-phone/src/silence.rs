//! Silence and idle policy for a connected call (#1556). Without this the
//! line stays open through the provider's duration limit whenever the owner
//! stops talking. Two spoken check-ins are followed by a fixed goodbye and
//! the #1557 `end` machinery with reason `silence`. The clock is passive: the
//! relay loop resets it on owner activity and on anything Jarvis speaks, and
//! polls it from the loop's existing tick.
use crate::Error;
use std::time::Duration;
use tokio::time::Instant;

pub const CHECK_IN_1_LINE: &str = "Still there?";
pub const CHECK_IN_2_LINE: &str = "I'll hang up in a bit if you're done.";
pub const GOODBYE_LINE: &str = "Looks like you're done. I'll hang up now. Bye!";
/// Reported in the originating chat once a call ended on silence.
pub const CHAT_NOTICE: &str = "Phone — the call ended because the line went quiet.";
/// Idempotency key suffix for [`CHAT_NOTICE`], scoped by call ID.
pub const CHAT_NOTICE_KEY: &str = "silence_end";
/// A dispatched task suspends the silence policy only while its last spoken
/// progress line is this recent: the owner is waiting, not absent.
pub const PROGRESS_SUSPENDS: Duration = Duration::from_secs(15);

/// What the silence clock asks the relay loop to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SilenceAction {
    CheckIn1,
    CheckIn2,
    /// Speak [`GOODBYE_LINE`] and end the call.
    Goodbye,
}
impl SilenceAction {
    pub fn line(self) -> &'static str {
        match self {
            Self::CheckIn1 => CHECK_IN_1_LINE,
            Self::CheckIn2 => CHECK_IN_2_LINE,
            Self::Goodbye => GOODBYE_LINE,
        }
    }
}

/// One threshold per [`SilenceAction`], measured from the last owner activity
/// or closing speech frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SilencePolicy([Duration; 3]);
impl Default for SilencePolicy {
    fn default() -> Self {
        Self::new(12, 25, 45).expect("the documented defaults are in range")
    }
}
impl SilencePolicy {
    /// Each threshold 5–300 s and strictly increasing. Errors name the
    /// operator variable, never the value.
    pub fn new(check_in_1_s: u64, check_in_2_s: u64, goodbye_s: u64) -> Result<Self, Error> {
        use crate::config::{SILENCE_CHECKIN_1_S, SILENCE_CHECKIN_2_S, SILENCE_HANGUP_S};
        let settings = [
            (check_in_1_s, SILENCE_CHECKIN_1_S),
            (check_in_2_s, SILENCE_CHECKIN_2_S),
            (goodbye_s, SILENCE_HANGUP_S),
        ];
        let mut thresholds = [Duration::ZERO; 3];
        for (stage, (seconds, key)) in settings.into_iter().enumerate() {
            let threshold = Duration::from_secs(seconds);
            // In range, and strictly past the stage before it.
            if !(5..=300).contains(&seconds) || (stage > 0 && threshold <= thresholds[stage - 1]) {
                return Err(Error::InvalidSetting(key));
            }
            thresholds[stage] = threshold;
        }
        Ok(Self(thresholds))
    }
}

/// Silence state of one connected call. Each stage fires at most once per
/// quiet stretch; a reset starts the sequence over.
#[derive(Debug)]
pub struct SilenceClock {
    policy: SilencePolicy,
    epoch: Instant,
    stage: u8,
}
impl SilenceClock {
    pub fn new(policy: SilencePolicy) -> Self {
        Self { policy, epoch: Instant::now(), stage: 0 }
    }
    /// Owner activity, a closing speech frame, a tokens-played event, or a
    /// tick on which silence handling is suspended: the line is not quiet, so
    /// the sequence starts over.
    pub fn reset(&mut self) {
        self.epoch = Instant::now();
        self.stage = 0;
    }
    /// The next due action, each returned once per quiet stretch.
    pub fn poll(&mut self) -> Option<SilenceAction> {
        let threshold = *self.policy.0.get(usize::from(self.stage))?;
        if self.epoch.elapsed() < threshold {
            return None;
        }
        self.stage += 1;
        Some(match self.stage {
            1 => SilenceAction::CheckIn1,
            2 => SilenceAction::CheckIn2,
            _ => SilenceAction::Goodbye,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SILENCE_CHECKIN_1_S, SILENCE_CHECKIN_2_S, SILENCE_HANGUP_S};

    /// AC4: a threshold outside 5–300 s, or a triple that is not strictly
    /// increasing, fails naming its own operator variable.
    #[test]
    fn validation_names_the_offending_variable() {
        for (key, triple) in [
            (SILENCE_CHECKIN_1_S, (4, 25, 45)),
            (SILENCE_CHECKIN_2_S, (12, 301, 400)),
            (SILENCE_HANGUP_S, (12, 25, 0)),
            (SILENCE_CHECKIN_2_S, (12, 12, 45)),
            (SILENCE_HANGUP_S, (12, 45, 25)),
        ] {
            match SilencePolicy::new(triple.0, triple.1, triple.2) {
                Err(Error::InvalidSetting(named)) => assert_eq!(named, key, "{triple:?}"),
                other => panic!("{triple:?} gave {other:?}"),
            }
        }
        assert!(SilencePolicy::new(5, 6, 300).is_ok(), "the bounds are inclusive");
    }

    /// AC1 / AC2 / AC3 arithmetic: the three stages fire once each at their
    /// thresholds, measured from the last reset; resetting on every tick
    /// (silence handling suspended) never lets a stage come due.
    #[tokio::test(start_paused = true)]
    async fn stages_fire_once_each_measured_from_the_last_reset() {
        let mut clock = SilenceClock::new(SilencePolicy::default());
        for _ in 0..300 {
            clock.reset();
            assert_eq!(clock.poll(), None, "a suspended clock never comes due");
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        clock.reset();
        let start = Instant::now();
        let mut fired = Vec::new();
        for _ in 0..600 {
            if let Some(action) = clock.poll() {
                fired.push((start.elapsed(), action));
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        assert_eq!(
            fired,
            [
                (Duration::from_secs(12), SilenceAction::CheckIn1),
                (Duration::from_secs(25), SilenceAction::CheckIn2),
                (Duration::from_secs(45), SilenceAction::Goodbye),
            ]
        );
    }
}
