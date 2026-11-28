//! #2389 — parking a Gmail compose the in-loop 429 backoff could not push
//! through.
//!
//! The in-loop backoff (#1486) already retries an envelope throttle a bounded
//! number of times and, when every attempt is refused, surfaces the typed
//! [`GmailError::RateLimited`](crate::gmail::GmailError::RateLimited) carrying
//! the provider's own retry boundary. That boundary is usually further out
//! than any in-loop backoff can wait, so the compose is lost.
//!
//! This module turns that exhausted throttle into a durable row (#2387): the
//! complete replay input, serialized, keyed so a repeated dispatch of the same
//! compose cannot park a second copy, and due at the provider's boundary
//! rather than at a poll interval. It is opt-in — with
//! [`ENV_ENABLED`] unset the channel surfaces the typed error exactly as
//! before.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use augmentagent_store::{NewComposeQueueEntry, Store, StoreError, StoreResult};

pub const ENV_ENABLED: &str = "AUGMENTAGENT_GMAIL_COMPOSE_QUEUE";
pub const ENV_MAX_ATTEMPTS: &str = "AUGMENTAGENT_GMAIL_COMPOSE_RETRY_MAX_ATTEMPTS";
pub const ENV_DEADLINE_MINS: &str = "AUGMENTAGENT_GMAIL_COMPOSE_RETRY_DEADLINE_MINS";

pub const DEFAULT_MAX_ATTEMPTS: i64 = 5;
pub const DEFAULT_DEADLINE_MINS: u64 = 120;

/// The replay payload shape this build writes and understands. A sweep that
/// reads a version it does not know refuses the row instead of composing a
/// partial draft.
pub const REPLAY_VERSION: u8 = 1;

/// How the channel treats a compose whose in-loop retries are exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComposeQueueConfig {
    /// `false` (the default) keeps the typed `RateLimited` as the only
    /// outcome, so an operator who has not opted in sees no behavior change.
    pub enabled: bool,
    /// Attempts a parked compose gets before it is given up on.
    pub max_attempts: i64,
    /// How long after parking the compose stops being worth attempting.
    pub deadline: Duration,
}

impl Default for ComposeQueueConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            deadline: Duration::from_secs(DEFAULT_DEADLINE_MINS * 60),
        }
    }
}

impl ComposeQueueConfig {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok();
        Self::from_raw(
            var(ENV_ENABLED).as_deref(),
            var(ENV_MAX_ATTEMPTS).as_deref(),
            var(ENV_DEADLINE_MINS).as_deref(),
        )
    }

    /// Env parsing without the process-global env, so the defaults and the
    /// accepted spellings are testable. An unparseable override keeps its
    /// default rather than disabling the queue an operator asked for.
    pub fn from_raw(
        enabled: Option<&str>,
        max_attempts: Option<&str>,
        deadline_mins: Option<&str>,
    ) -> Self {
        let mut cfg = Self {
            enabled: matches!(
                enabled.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
                Some("1" | "true" | "yes" | "on")
            ),
            ..Self::default()
        };
        if let Some(v) = max_attempts.and_then(|v| v.trim().parse::<i64>().ok()).filter(|v| *v > 0) {
            cfg.max_attempts = v;
        }
        if let Some(v) = deadline_mins.and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v > 0) {
            cfg.deadline = Duration::from_secs(v * 60);
        }
        cfg
    }
}

/// Everything a later attempt needs to recreate the throttled draft without
/// re-running triage or the drafter. `draft_body` is the card-marker-carrying
/// body held in `actions.draftBody`; `body` is the already-scrubbed text
/// `create_draft_with_cc` was called with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeReplay {
    pub version: u8,
    pub entity_id: String,
    pub action_id: String,
    pub message_id: String,
    pub to: String,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub draft_body: String,
    pub thread_id: Option<String>,
    pub max_attempts: i64,
    /// Wallclock ms after which the compose is no longer worth attempting.
    pub deadline_ms: i64,
}

/// What a parked compose is keyed by. Derived from the compose itself — never
/// from the action row id, which a repeated dispatch of the same message
/// allocates afresh — so the second dispatch collides with the first instead
/// of parking a duplicate.
pub fn compose_idem_key(replay: &ComposeReplay) -> String {
    use md5::{Digest, Md5};
    let mut digest = Md5::new();
    for part in [
        replay.to.as_str(),
        &replay.cc.join(","),
        replay.subject.as_str(),
        replay.body.as_str(),
        replay.thread_id.as_deref().unwrap_or(""),
    ] {
        digest.update(part.as_bytes());
        digest.update([0u8]);
    }
    format!(
        "gmail-compose:{}:{}:{:x}",
        replay.entity_id,
        replay.message_id,
        digest.finalize()
    )
}

/// The key a compose is parked under, plus whether this call is what parked
/// it. `fresh = false` means an identical compose was already queued and the
/// stored row — payload, boundary and attempt count — wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedCompose {
    pub idem_key: String,
    pub fresh: bool,
}

/// Park `replay` at `retry_after_ms`, the provider's own boundary. A failure
/// to persist is returned, never swallowed: the caller must not report a
/// compose as queued that is not in the file.
pub fn enqueue_exhausted_compose(
    store: &Store,
    replay: &ComposeReplay,
    retry_after_ms: i64,
    now_ms: i64,
) -> StoreResult<QueuedCompose> {
    let idem_key = compose_idem_key(replay);
    let payload = serde_json::to_string(replay)
        .map_err(|e| StoreError::InvalidInput(format!("serialize compose replay {idem_key}: {e}")))?;
    let fresh = store.enqueue_gmail_compose(
        &NewComposeQueueEntry {
            entity: replay.entity_id.clone(),
            payload,
            retry_after_ms,
            idem_key: idem_key.clone(),
        },
        now_ms,
    )?;
    Ok(QueuedCompose { idem_key, fresh })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_config_defaults_to_off_with_five_attempts_and_a_two_hour_deadline() {
        let cfg = ComposeQueueConfig::from_raw(None, None, None);
        assert_eq!(cfg, ComposeQueueConfig::default());
        assert!(!cfg.enabled, "the queue must stay opt-in");
        assert_eq!(cfg.max_attempts, 5);
        assert_eq!(cfg.deadline, Duration::from_secs(120 * 60));
    }

    #[test]
    fn queue_config_reads_explicit_values_and_keeps_defaults_for_junk() {
        let cfg = ComposeQueueConfig::from_raw(Some(" TRUE "), Some("3"), Some("45"));
        assert!(cfg.enabled);
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.deadline, Duration::from_secs(45 * 60));

        for off in ["0", "false", "no", "", "maybe"] {
            assert!(!ComposeQueueConfig::from_raw(Some(off), None, None).enabled, "{off}");
        }
        let junk = ComposeQueueConfig::from_raw(Some("1"), Some("nope"), Some("0"));
        assert!(junk.enabled);
        assert_eq!(junk.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(junk.deadline, Duration::from_secs(DEFAULT_DEADLINE_MINS * 60));
    }

    #[test]
    fn the_idempotency_key_ignores_the_action_row_but_separates_revised_bodies() {
        let replay = ComposeReplay {
            version: REPLAY_VERSION,
            entity_id: "acc1".into(),
            action_id: "action-first".into(),
            message_id: "m-throttled".into(),
            to: "person@example.com".into(),
            cc: vec!["other@example.org".into()],
            subject: "Re: quick q".into(),
            body: "Thursday works.".into(),
            draft_body: "Thursday works.".into(),
            thread_id: Some("t-throttled".into()),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            deadline_ms: 1_000,
        };
        let redispatched = ComposeReplay { action_id: "action-second".into(), deadline_ms: 9_000, ..replay.clone() };
        assert_eq!(compose_idem_key(&replay), compose_idem_key(&redispatched));

        let revised = ComposeReplay { body: "Friday works.".into(), ..replay.clone() };
        assert_ne!(compose_idem_key(&replay), compose_idem_key(&revised));
    }
}
