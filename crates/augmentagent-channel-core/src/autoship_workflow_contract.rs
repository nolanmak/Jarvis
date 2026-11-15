//! Shared public-safe workflow records for #1703, owned by S (#1706).
//!
//! These are data contracts, not execution, review or merge authority. Producers
//! validate before committing; sequence allocation, leases, transactional event/
//! outbox writes, readiness and merge receipts remain with their owning services.
//! Native session identifiers, transcripts, credentials and private evidence
//! bodies never belong here. Binding values are opaque public-safe references.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOutcome {
    Progressed,
    DependencyWait,
    QuotaWait,
    CapacityWait,
    ConfigurationFault,
    RetryableInfrastructure,
    CodeOrReviewFailure,
}

/// Wire names match the existing durable workflow stages. No store migration or
/// runtime adoption is performed by publishing this contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStage {
    Intake,
    Scope,
    Build,
    Verify,
    Review,
    Reconcile,
    Publish,
    AwaitChecks,
    Merge,
    Held,
    Retired,
    Merged,
}

/// A stage start alone is not evidence that an unchanged failure was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressTransition {
    Observation,
    StageChanged,
    MeaningfulProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Controller,
    Owner,
    Builder,
    Reviewer,
    Adjudicator,
    Github,
    Updater,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Poller,
    Cli,
    Discord,
    Resume,
    MergeSweep,
    Reconciliation,
    Updater,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "number", rename_all = "snake_case")]
pub enum WorkflowTarget {
    Issue(u64),
    PullRequest(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub head: String,
    pub contract_version: String,
    pub evidence_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextAction {
    /// Controller machine code, never arbitrary model text.
    pub action: String,
    /// Unix milliseconds in UTC; None when there is no scheduled action.
    pub eligible_at_ms: Option<i64>,
}

/// Cumulative workflow usage snapshots: do not sum successive events. Spending
/// and progress are independent, and none of these values is a merge debit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeUsage {
    pub model_calls: u64,
    pub tokens: u64,
    pub wall_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEvent {
    pub schema_version: u32,
    pub workflow_id: String,
    /// Positive, monotonically increasing within this workflow. Consumers
    /// deduplicate by (workflow_id, sequence), not head or actor.
    pub sequence: i64,
    pub occurred_at_ms: i64,
    /// Lowercase canonical owner/name supplied by the repository authority.
    pub repository: String,
    pub target: WorkflowTarget,
    pub pull_request: Option<u64>,
    pub binding: Binding,
    pub stage: WorkflowStage,
    pub outcome: WorkflowOutcome,
    pub transition: ProgressTransition,
    pub reason_code: String,
    pub last_meaningful_progress_at_ms: i64,
    pub next: NextAction,
    pub actor: Actor,
    pub origin: Origin,
    pub compute_usage: ComputeUsage,
}

impl WorkflowEvent {
    /// Validate a wire record before accepting it. This does not authenticate
    /// evidence or attest that a remote mutation actually happened.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported workflow schema version"
        );
        let parts = self.repository.split('/').collect::<Vec<_>>();
        ensure!(
            parts.len() == 2
                && parts.iter().all(|part| !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b"._-".contains(&b))),
            "canonical repository required"
        );
        let expected = match self.target {
            WorkflowTarget::Issue(number) => {
                ensure!(number > 0, "issue identity required");
                format!("{}#{number}", self.repository)
            }
            WorkflowTarget::PullRequest(number) => {
                ensure!(
                    number > 0 && self.pull_request.is_none_or(|pr| pr == number),
                    "PR identity differs"
                );
                format!("{}/pr/{number}", self.repository)
            }
        };
        ensure!(
            self.workflow_id == expected && self.pull_request.is_none_or(|pr| pr > 0),
            "workflow identity differs"
        );
        ensure!(self.sequence > 0, "positive workflow sequence required");
        ensure!(
            self.occurred_at_ms >= 0
                && (0..=self.occurred_at_ms).contains(&self.last_meaningful_progress_at_ms)
                && self.next.eligible_at_ms.is_none_or(|at| at >= 0),
            "invalid UTC timestamp"
        );
        ensure!(
            machine_code(&self.reason_code) && machine_code(&self.next.action),
            "machine reason and action required"
        );
        ensure!(
            [
                &self.binding.head,
                &self.binding.contract_version,
                &self.binding.evidence_version
            ]
            .iter()
            .all(|value| !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))),
            "bounded evidence binding required"
        );
        Ok(())
    }

    /// Check a successive record without performing a transition or allocating
    /// a sequence. The writer still owns the atomic event/outbox transaction.
    pub fn validate_after(&self, previous: &Self) -> Result<()> {
        previous.validate()?;
        self.validate()?;
        ensure!(
            self.workflow_id == previous.workflow_id
                && self.target == previous.target
                && self.repository == previous.repository,
            "transition changed workflow identity"
        );
        ensure!(
            self.sequence > previous.sequence,
            "workflow sequence did not advance"
        );
        ensure!(
            self.compute_usage.model_calls >= previous.compute_usage.model_calls
                && self.compute_usage.tokens >= previous.compute_usage.tokens
                && self.compute_usage.wall_ms >= previous.compute_usage.wall_ms,
            "cumulative workflow usage decreased"
        );
        match self.transition {
            ProgressTransition::Observation => {
                ensure!(self.stage == previous.stage, "observation changed stage");
            }
            ProgressTransition::StageChanged => {
                ensure!(
                    self.stage != previous.stage,
                    "stage-change record did not change stage"
                );
            }
            ProgressTransition::MeaningfulProgress => {
                ensure!(
                    self.last_meaningful_progress_at_ms == self.occurred_at_ms
                        && self.last_meaningful_progress_at_ms
                            >= previous.last_meaningful_progress_at_ms,
                    "meaningful progress requires the current controller timestamp"
                );
                return Ok(());
            }
        }
        ensure!(
            self.last_meaningful_progress_at_ms == previous.last_meaningful_progress_at_ms,
            "observation or stage change altered meaningful progress"
        );
        Ok(())
    }
}

fn machine_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.:".contains(&b))
}
