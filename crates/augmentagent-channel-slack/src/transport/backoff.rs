//! Bounded exponential backoff with jitter for reconnects.

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackoffConfig {
    /// Delay for the first retry, before jitter.
    pub initial: Duration,
    /// Hard cap on any single delay.
    pub max: Duration,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

impl BackoffConfig {
    /// Delay before retry number `attempt` (1-based). `jitter` is a value in
    /// `[0, 1)`; the result lies in `[cap/2, cap]` where
    /// `cap = min(max, initial * 2^(attempt-1))`, so it is never zero and
    /// never above `max`.
    pub fn delay(&self, attempt: u32, jitter: f64) -> Duration {
        let exp = attempt.saturating_sub(1).min(31);
        let cap = self.initial.saturating_mul(1u32 << exp).min(self.max);
        let jitter = jitter.clamp(0.0, 1.0);
        cap.mul_f64(0.5 + 0.5 * jitter).min(self.max)
    }
}

/// A jitter value in `[0, 1)` derived from the process's CSPRNG through the
/// `uuid` crate the workspace already depends on, so no extra RNG crate.
pub fn random_jitter() -> f64 {
    let bits = uuid::Uuid::new_v4().as_u128() as u64 >> 11;
    bits as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_grows_exponentially_and_is_bounded() {
        let b = BackoffConfig {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(8),
        };
        assert_eq!(b.delay(1, 1.0), Duration::from_secs(1));
        assert_eq!(b.delay(2, 1.0), Duration::from_secs(2));
        assert_eq!(b.delay(3, 1.0), Duration::from_secs(4));
        assert_eq!(b.delay(4, 1.0), Duration::from_secs(8));
        assert_eq!(
            b.delay(40, 1.0),
            Duration::from_secs(8),
            "capped, no overflow"
        );
        assert_eq!(
            b.delay(3, 0.0),
            Duration::from_secs(2),
            "half the cap at minimum"
        );
        assert!(b.delay(0, 0.5) >= Duration::from_millis(500));
    }

    #[test]
    fn random_jitter_is_in_unit_interval() {
        for _ in 0..1000 {
            let j = random_jitter();
            assert!((0.0..1.0).contains(&j), "{j}");
        }
    }
}
