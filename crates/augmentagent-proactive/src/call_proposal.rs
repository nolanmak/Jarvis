//! Agent-proposed calls (#1553), as seen by proactive loops. A loop may only
//! *ask*: [`CallProposer::propose_call`] hands a reason and a short agenda to
//! the phone service, which posts a yes/no question to the owner's private
//! control chat (or defers it past quiet hours). Dialing, owner authority and
//! rate limits live in the phone service; this crate has no path to them.
use async_trait::async_trait;

/// Agenda bullets carried into the first phone turn.
pub const AGENDA_MAX: usize = 5;
const REASON_MAX: usize = 300;
const ITEM_MAX: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallProposal {
    pub reason: String,
    /// At most [`AGENDA_MAX`] single-line items.
    pub agenda: Vec<String>,
}
impl CallProposal {
    /// One proposal covering up to five headlines; `None` when there are none.
    pub fn from_headlines(headlines: &[String]) -> Option<Self> {
        let headlines: Vec<String> = headlines
            .iter()
            .map(|h| bounded(h, ITEM_MAX))
            .filter(|h| !h.is_empty())
            .collect();
        if headlines.is_empty() {
            return None;
        }
        let n = headlines.len();
        let reason = if n == 1 {
            "A follow-up needs your attention".to_owned()
        } else {
            format!("{n} follow-ups need your attention")
        };
        Some(Self {
            reason: bounded(&reason, REASON_MAX),
            agenda: headlines.into_iter().take(AGENDA_MAX).collect(),
        })
    }
}
fn bounded(text: &str, max: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

/// When the question will reach the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalOutcome {
    pub deliver_at_ms: i64,
    /// Quiet hours moved delivery later.
    pub deferred: bool,
}
#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
    /// At most one proposal per 4 hours and three per day.
    #[error("call proposal rate limit reached")]
    RateLimited,
    #[error("call proposals are unavailable: {0}")]
    Unavailable(String),
}

/// Implemented by the daemon over the phone service. Never dials.
#[async_trait]
pub trait CallProposer: Send + Sync {
    async fn propose_call(&self, proposal: CallProposal) -> Result<ProposalOutcome, ProposalError>;
}
