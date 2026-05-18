//! `ScheduledScan` trait + the value types every rule produces.
//!
//! A scan is a pure read over the wiki + sqlite store: it walks `people/*.md`
//! and emits zero or more [`ProactiveSignal`]s. The runner is the only thing
//! that mutates state — it persists signals via [`crate::store_ext`] and
//! dispatches the fresh ones through the existing `ApprovalBroker`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_store::Store;
use augmentagent_wiki::WikiLayout;
use serde::{Deserialize, Serialize};

/// How often a rule wants to be run. The runner ticks every 30 min; a rule
/// whose cadence is coarser than the tick is re-evaluated each tick and dedup
/// (see [`ProactiveSignal::dedup_key`]) keeps it from spamming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    EveryTick,
    Hourly,
    Daily,
    Weekly,
}

impl Cadence {
    /// Minimum spacing, in ms, between two signals sharing a dedup key.
    pub fn dedup_window_ms(self) -> i64 {
        match self {
            Self::EveryTick => 25 * 60 * 1000,
            Self::Hourly => 55 * 60 * 1000,
            Self::Daily => 23 * 60 * 60 * 1000,
            Self::Weekly => 6 * 24 * 60 * 60 * 1000,
        }
    }
}

/// Triage weight for a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    Normal,
    High,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "high" => Self::High,
            "low" => Self::Low,
            _ => Self::Normal,
        }
    }
}

/// Which rule produced a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    StaleContact,
    StaleCommitment,
    EventReminder,
}

impl SignalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StaleContact => "stale_contact",
            Self::StaleCommitment => "stale_commitment",
            Self::EventReminder => "event_reminder",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stale_contact" => Some(Self::StaleContact),
            "stale_commitment" => Some(Self::StaleCommitment),
            "event_reminder" => Some(Self::EventReminder),
            _ => None,
        }
    }
}

/// A concrete next step the user could take, surfaced on the signal card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedAction {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_prompt: Option<String>,
}

/// One emitted signal. `id` is filled by the store on insert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProactiveSignal {
    #[serde(default)]
    pub id: String,
    pub kind: SignalKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_slug: Option<String>,
    pub urgency: Urgency,
    pub headline: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<SuggestedAction>,
    pub dedup_key: String,
}

impl ProactiveSignal {
    pub fn new(
        kind: SignalKind,
        urgency: Urgency,
        headline: impl Into<String>,
        detail: impl Into<String>,
        dedup_key: impl Into<String>,
    ) -> Self {
        Self {
            id: String::new(),
            kind,
            person_slug: None,
            urgency,
            headline: headline.into(),
            detail: detail.into(),
            suggested_action: None,
            dedup_key: dedup_key.into(),
        }
    }
    pub fn with_person(mut self, slug: impl Into<String>) -> Self {
        self.person_slug = Some(slug.into());
        self
    }
    pub fn with_action(mut self, action: SuggestedAction) -> Self {
        self.suggested_action = Some(action);
        self
    }
}

/// Read-only context handed to every scan.
pub struct ScanCtx {
    pub store: Arc<Store>,
    pub wiki: WikiLayout,
    pub now_ms: i64,
}

impl ScanCtx {
    pub fn new(store: Arc<Store>, wiki_root: PathBuf, now_ms: i64) -> Self {
        Self {
            store,
            wiki: WikiLayout::new(wiki_root),
            now_ms,
        }
    }
}

/// A scheduled, read-only scan over wiki + store.
#[async_trait]
pub trait ScheduledScan: Send + Sync {
    fn id(&self) -> &'static str;
    fn cadence(&self) -> Cadence;
    async fn scan(&self, ctx: &ScanCtx) -> anyhow::Result<Vec<ProactiveSignal>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_kind_roundtrips() {
        for k in [
            SignalKind::StaleContact,
            SignalKind::StaleCommitment,
            SignalKind::EventReminder,
        ] {
            assert_eq!(SignalKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(SignalKind::parse("bogus"), None);
    }

    #[test]
    fn urgency_roundtrips() {
        for u in [Urgency::Low, Urgency::Normal, Urgency::High] {
            assert_eq!(Urgency::parse(u.as_str()), u);
        }
        assert_eq!(Urgency::parse("???"), Urgency::Normal);
    }

    #[test]
    fn dedup_windows_are_ordered() {
        assert!(Cadence::EveryTick.dedup_window_ms() < Cadence::Hourly.dedup_window_ms());
        assert!(Cadence::Hourly.dedup_window_ms() < Cadence::Daily.dedup_window_ms());
        assert!(Cadence::Daily.dedup_window_ms() < Cadence::Weekly.dedup_window_ms());
    }

    #[test]
    fn builder_sets_fields() {
        let s = ProactiveSignal::new(
            SignalKind::StaleContact,
            Urgency::Normal,
            "head",
            "detail",
            "dedup-1",
        )
        .with_person("jane_at_corp_com")
        .with_action(SuggestedAction {
            label: "Draft check-in".into(),
            draft_prompt: Some("Reconnect with Jane".into()),
        });
        assert_eq!(s.person_slug.as_deref(), Some("jane_at_corp_com"));
        assert_eq!(s.suggested_action.unwrap().label, "Draft check-in");
        assert!(s.id.is_empty());
    }
}
