//! #1537 (A) — one normalized ownership attestation for a PR autoship may act
//! on, whatever its branch name.
//!
//! Ownership comes from exactly two durable authorities: the loop's own
//! publication record (legacy `agent-fix/issue-N` work) or an authorized
//! adoption record (same-repository PR by a trusted author, via the poller's
//! default policy or an explicit owner command). Branch spelling, approval
//! prose and body markers never create it. Source linkage is verified against
//! GitHub's structured closing references at use time; a missing, changed or
//! ambiguous linkage is reported as such and never resolved by guessing.
//! Head movement invalidates review, not this ownership.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdoptionOrigin {
    /// The loop published this PR itself (opened-PR publication record).
    Publication,
    /// Observed by the poller under the default same-repository author policy.
    AuthorizedPolicy,
    /// Submitted through an authenticated owner command (CLI/Discord).
    OwnerCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum SourceLinkage {
    /// Verified: the recorded source still matches GitHub's structured link.
    Issue { number: u64 },
    /// The PR has no source issue; it is judged by its own completion state.
    SourceLess,
    /// More than one structured source; retirement needs an explicit decision.
    Ambiguous { issues: Vec<u64> },
    /// The structured source differs from the adopted record.
    Changed { recorded: Option<u64>, current: Vec<u64> },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct OwnershipAttestation {
    /// Lowercase canonical owner/name. With `pr` this is the immutable identity.
    pub repository: String,
    pub pr: u64,
    pub branch: String,
    pub origin: AdoptionOrigin,
    pub source: SourceLinkage,
}

impl OwnershipAttestation {
    /// The verified source issue, if and only if linkage is intact.
    pub(crate) fn source_issue(&self) -> Option<u64> {
        match self.source {
            SourceLinkage::Issue { number } => Some(number),
            _ => None,
        }
    }
}

/// Why a live PR no longer matches its adopted ownership record.
pub(crate) const OWNERSHIP_CHANGED: &str = "adopted-ownership-changed";

/// Attest an adopted PR against its live GitHub row (`number`,
/// `headRefName`, `isCrossRepository`, `closingIssuesReferences`).
pub(super) fn attest_adopted(
    record: &adoption::AdoptedPr,
    live: &serde_json::Value,
    origin: AdoptionOrigin,
) -> std::result::Result<OwnershipAttestation, &'static str> {
    if live["number"].as_u64() != Some(record.pr)
        || live["isCrossRepository"] != false
        || live["headRefName"].as_str() != Some(record.branch.as_str())
    {
        return Err(OWNERSHIP_CHANGED);
    }
    let current = adoption::source_references(live, &record.repository);
    let source = match (record.source_issue, current.as_slice()) {
        (_, [_, _, ..]) => SourceLinkage::Ambiguous { issues: current.clone() },
        (Some(recorded), [one]) if recorded == *one => SourceLinkage::Issue { number: recorded },
        // Legacy harness branches carry their source in the branch itself.
        (Some(recorded), []) if issue_from_branch(&record.branch) == Some(recorded) => {
            SourceLinkage::Issue { number: recorded }
        }
        (None, []) => SourceLinkage::SourceLess,
        (recorded, _) => SourceLinkage::Changed { recorded, current: current.clone() },
    };
    Ok(OwnershipAttestation {
        repository: record.repository.clone(),
        pr: record.pr,
        branch: record.branch.clone(),
        origin,
        source,
    })
}

/// The durable adoption record and its origin, or `None` when autoship never
/// adopted this PR. An unreadable store is an error, never absence.
pub(super) fn adopted(
    path: &Path,
    repository: &str,
    pr: u64,
) -> Result<Option<(adoption::AdoptedPr, AdoptionOrigin)>> {
    let repository = repository.to_ascii_lowercase();
    let Some(record) = adoption::lookup(path, &repository, pr)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        record.repository == repository && record.pr == pr,
        "adoption record identity differs"
    );
    let workflow = crate::autoship_workflow::pr_workflow_id(&repository, pr)?;
    let command = crate::autoship_workflow::WorkflowStore::open(path)?
        .command_request(&workflow)?
        .is_some();
    let origin = if command {
        AdoptionOrigin::OwnerCommand
    } else {
        AdoptionOrigin::AuthorizedPolicy
    };
    Ok(Some((record, origin)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(branch: &str, source: Option<u64>) -> adoption::AdoptedPr {
        adoption::AdoptedPr {
            repository: "owner/repo".into(),
            pr: 42,
            source_issue: source,
            branch: branch.into(),
            head: "a".repeat(40),
            base: "main".into(),
            author: "owner".into(),
            title: "t".into(),
            body: "b".into(),
            dependency: adoption::Dependency::Main,
        }
    }
    fn live(branch: &str, issues: &[u64]) -> serde_json::Value {
        serde_json::json!({"number":42,"headRefName":branch,"isCrossRepository":false,
            "closingIssuesReferences":issues.iter().map(|n| serde_json::json!({"number":n,"url":format!("https://github.com/owner/repo/issues/{n}")})).collect::<Vec<_>>()})
    }
    fn source(record: &adoption::AdoptedPr, live: &serde_json::Value) -> SourceLinkage {
        attest_adopted(record, live, AdoptionOrigin::AuthorizedPolicy).unwrap().source
    }

    #[test]
    fn linkage_is_verified_and_never_guessed() {
        assert_eq!(source(&record("feat/x", Some(7)), &live("feat/x", &[7])), SourceLinkage::Issue { number: 7 });
        assert_eq!(source(&record("agent-fix/issue-7", Some(7)), &live("agent-fix/issue-7", &[])), SourceLinkage::Issue { number: 7 });
        assert_eq!(source(&record("feat/x", None), &live("feat/x", &[])), SourceLinkage::SourceLess);
        assert_eq!(source(&record("feat/x", None), &live("feat/x", &[7, 8])), SourceLinkage::Ambiguous { issues: vec![7, 8] });
        assert_eq!(source(&record("feat/x", Some(7)), &live("feat/x", &[8])),
            SourceLinkage::Changed { recorded: Some(7), current: vec![8] });
        // A removed closing reference on a non-legacy branch is not inferred.
        assert_eq!(source(&record("feat/x", Some(7)), &live("feat/x", &[])),
            SourceLinkage::Changed { recorded: Some(7), current: vec![] });
        // A source that appears after adoption must be re-observed first.
        assert_eq!(source(&record("feat/x", None), &live("feat/x", &[7])),
            SourceLinkage::Changed { recorded: None, current: vec![7] });
        // Foreign-repository references are not this repository's sources.
        let mut foreign = live("feat/x", &[]);
        foreign["closingIssuesReferences"] = serde_json::json!([{"number":7,"url":"https://github.com/other/repo/issues/7"}]);
        assert_eq!(source(&record("feat/x", None), &foreign), SourceLinkage::SourceLess);
    }

    #[test]
    fn renamed_forked_or_renumbered_heads_are_not_the_adopted_pr() {
        let owned = record("feat/x", Some(7));
        assert_eq!(attest_adopted(&owned, &live("feat/renamed", &[7]), AdoptionOrigin::OwnerCommand), Err(OWNERSHIP_CHANGED));
        let mut fork = live("feat/x", &[7]);
        fork["isCrossRepository"] = true.into();
        assert_eq!(attest_adopted(&owned, &fork, AdoptionOrigin::OwnerCommand), Err(OWNERSHIP_CHANGED));
        let mut other = live("feat/x", &[7]);
        other["number"] = 43.into();
        assert_eq!(attest_adopted(&owned, &other, AdoptionOrigin::OwnerCommand), Err(OWNERSHIP_CHANGED));
        let attested = attest_adopted(&owned, &live("feat/x", &[7]), AdoptionOrigin::OwnerCommand).unwrap();
        assert_eq!((attested.source_issue(), attested.origin.clone()), (Some(7), AdoptionOrigin::OwnerCommand));
        assert_eq!(serde_json::to_value(&attested).unwrap()["source"], serde_json::json!({"kind":"issue","number":7}));
    }
}
