//! Triage pre-filter hook (#1127).
//!
//! An optional, pluggable step that runs before the triage reasoner call and
//! may decide a message is routine enough to skip without a model call. The
//! verdict type can only express `Skip`: a pre-filter can never reply, draft,
//! flag or raise a card. A sampled share of its decisions still goes to the
//! reasoner (spot-checks) so the pre-filter's agreement with real triage is
//! measured continuously; implementations use that to disable themselves.

use async_trait::async_trait;
use augmentagent_channel_core::decision::DecisionKind;
use augmentagent_store::Email;

/// The only decision a pre-filter may make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefilterVerdict {
    Skip,
}

impl PrefilterVerdict {
    pub fn as_decision(self) -> DecisionKind {
        match self {
            Self::Skip => DecisionKind::Skip,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrefilterDecision {
    pub verdict: PrefilterVerdict,
    /// Human-readable reason stored on the action row (`prefilter: …`).
    pub reason: String,
    /// Neighbours, similarities, thresholds: whatever makes the decision
    /// auditable. Stored by `record`, never shown to the model.
    pub audit: serde_json::Value,
}

#[async_trait]
pub trait TriagePrefilter: Send + Sync {
    /// `false` when switched off (env) or self-disabled (disagreement bound).
    fn enabled(&self) -> bool;
    /// A confident opinion about this message, or `None` for normal triage.
    async fn assess(&self, email: &Email) -> Option<PrefilterDecision>;
    /// Deterministic per message: should this one go to the reasoner anyway?
    fn is_spot_check(&self, message_id: &str) -> bool;
    /// Record what happened. `reasoner` is `Some` for spot-checks (what
    /// triage actually decided) and `None` when the pre-filter decided alone.
    async fn record(
        &self,
        message_id: &str,
        decision: &PrefilterDecision,
        reasoner: Option<DecisionKind>,
    );
}

/// Reason prefix on action rows decided by a pre-filter, so those rows can be
/// excluded from anything that learns from past decisions.
pub const REASON_PREFIX: &str = "prefilter:";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefilter_can_only_emit_low_stakes_decisions() {
        // The verdict enum has exactly one variant; this match is exhaustive
        // and would stop compiling if a reply/flag/draft variant were added.
        let v = PrefilterVerdict::Skip;
        match v {
            PrefilterVerdict::Skip => {}
        }
        assert_eq!(v.as_decision(), DecisionKind::Skip);
    }
}
