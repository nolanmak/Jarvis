//! Host-owned policy for dependency-enabled Code Mode computation.
mod dispatch;
pub mod service;
pub use dispatch::ComputeDispatcher;
pub use service::{ComputeService, ServiceConfig};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputePolicy {
    pub enabled: bool,
    pub call_timeout: Duration,
    pub task_timeout: Duration,
}

impl ComputePolicy {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let enabled = match lookup("AUGMENTAGENT_COMPUTE_ENABLED").as_deref() {
            None | Some("false") | Some("0") => false,
            Some("true") | Some("1") => true,
            _ => return Err("Invalid compute enable policy.".into()),
        };
        let seconds = |key: &str, default: u64, maximum: u64| -> Result<Duration, String> {
            let Some(raw) = lookup(key) else {
                return Ok(Duration::from_secs(default));
            };
            if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("Invalid {key}: expected bounded integer seconds."));
            }
            let value = raw
                .parse::<u64>()
                .map_err(|_| format!("Invalid {key}: integer overflow."))?;
            if !(1..=maximum).contains(&value) {
                return Err(format!("Invalid {key}: expected 1..={maximum}."));
            }
            Ok(Duration::from_secs(value))
        };
        Ok(Self {
            enabled,
            call_timeout: seconds("AUGMENTAGENT_COMPUTE_TIMEOUT_SECS", 600, 900)?,
            task_timeout: seconds("AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS", 1800, 3600)?,
        })
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn permits_owner_dm(&self, is_owner: bool) -> bool {
        self.enabled && is_owner
    }
}

/// One monotonic budget is owned by the originating task, including repair.
pub struct TaskBudget {
    policy: ComputePolicy,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    deadline: Instant,
}

impl TaskBudget {
    pub fn new(policy: ComputePolicy) -> Self {
        Self::with_clock(policy, Arc::new(Instant::now))
    }

    pub fn with_clock(
        policy: ComputePolicy,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        let deadline = clock() + policy.task_timeout;
        Self {
            policy,
            clock,
            deadline,
        }
    }

    pub fn remaining(&self) -> Result<Duration, &'static str> {
        // Round down for the Deno header; never extend a fractional remainder.
        let remaining = self.deadline.saturating_duration_since((self.clock)());
        let millis = remaining.as_millis() as u64;
        if millis == 0 {
            return Err("timeout");
        }
        Ok(Duration::from_millis(millis))
    }

    pub fn call_budget(&self, requested_secs: Option<u64>) -> Result<Duration, &'static str> {
        let seconds = requested_secs.unwrap_or(self.policy.call_timeout.as_secs());
        if seconds == 0 || seconds > self.policy.call_timeout.as_secs() {
            return Err("bad_args");
        }
        Ok(Duration::from_secs(seconds).min(self.remaining()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn policy(entries: &[(&str, &str)]) -> Result<ComputePolicy, String> {
        let map: BTreeMap<_, _> = entries.iter().copied().collect();
        ComputePolicy::from_lookup(|key| map.get(key).map(|v| (*v).to_owned()))
    }
    #[test]
    fn task_clock_does_not_reset_for_calls_or_repair() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let start = Instant::now();
        let elapsed = Arc::new(AtomicU64::new(0));
        let reader = Arc::clone(&elapsed);
        let p = policy(&[("AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS", "10")]).unwrap();
        let budget = TaskBudget::with_clock(
            p,
            Arc::new(move || start + Duration::from_millis(reader.load(Ordering::SeqCst))),
        );
        assert_eq!(budget.call_budget(None).unwrap(), Duration::from_secs(10));
        elapsed.store(7500, Ordering::SeqCst);
        assert_eq!(budget.remaining().unwrap(), Duration::from_millis(2500));
        assert_eq!(
            budget.call_budget(Some(6)).unwrap(),
            Duration::from_millis(2500)
        );
        elapsed.store(10000, Ordering::SeqCst);
        assert_eq!(budget.remaining(), Err("timeout"));
        assert_eq!(budget.call_budget(None), Err("timeout"));
    }

    #[test]
    fn call_cannot_expand_operator_budget() {
        let budget = TaskBudget::new(policy(&[]).unwrap());
        assert_eq!(budget.call_budget(Some(0)), Err("bad_args"));
        assert_eq!(budget.call_budget(Some(601)), Err("bad_args"));
        assert_eq!(budget.call_budget(Some(5)).unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn compute_is_opt_in_with_bounded_defaults() {
        let p = policy(&[]).unwrap();
        assert!(!p.enabled);
        assert_eq!(p.call_timeout.as_secs(), 600);
        assert_eq!(p.task_timeout.as_secs(), 1800);
        assert!(
            policy(&[("AUGMENTAGENT_COMPUTE_ENABLED", "true")])
                .unwrap()
                .enabled
        );
    }
    #[test]
    fn operator_budget_is_honored_without_clamping() {
        let p = policy(&[
            ("AUGMENTAGENT_COMPUTE_TIMEOUT_SECS", "90"),
            ("AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS", "120"),
        ])
        .unwrap();
        assert_eq!(p.call_timeout.as_secs(), 90);
        assert_eq!(p.task_timeout.as_secs(), 120);
    }
    #[test]
    fn malformed_operator_policy_fails_closed_even_when_disabled() {
        for (key, values) in [
            ("AUGMENTAGENT_COMPUTE_ENABLED", vec!["", "tru", "yes", "2"]),
            (
                "AUGMENTAGENT_COMPUTE_TIMEOUT_SECS",
                vec!["0", "-1", "901", "2.5", "true", "", " 5"],
            ),
            (
                "AUGMENTAGENT_CODE_MODE_COMPUTE_TIMEOUT_SECS",
                vec!["0", "3601", "-1", "1.5"],
            ),
        ] {
            for value in values {
                assert!(policy(&[(key, value)]).is_err(), "accepted {key}={value}");
            }
        }
    }
}
