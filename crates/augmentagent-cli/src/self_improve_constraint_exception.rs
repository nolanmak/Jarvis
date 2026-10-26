//! Owner contracts outrank learned rules. Exceptions require a separate fresh
//! authority and apply only to an exact aligned hunk, contract, policy and head.
use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Permit {
    rule_id: String,
    hunk_id: String,
    criterion_ids: Vec<String>,
    reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Exception {
    head: String,
    contract_hash: String,
    policy_hash: String,
    input_hash: String,
    provider: String,
    native_session: String,
    permits: Vec<Permit>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    head: String,
    contract_hash: String,
    policy_hash: String,
    permits: Vec<Permit>,
}
fn violations(rules: &[Constraint], evidence: &[HunkEvidence]) -> Result<Vec<(String, String)>> {
    let mut failures = vec![];
    for hunk in evidence {
        let added = hunk
            .patch
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .map(|line| line[1..].to_owned())
            .collect::<Vec<_>>();
        for rule in contract::constraint_failures(rules, &[hunk.hunk.path.clone()], &added)? {
            failures.push((rule, hunk.hunk.id.clone()));
        }
    }
    Ok(failures)
}
fn input_hash(evidence: &[HunkEvidence]) -> Result<String> {
    Ok(hash(&serde_json::to_vec(
        &evidence
            .iter()
            .map(|h| (&h.hunk.id, &h.hunk.path, &h.patch))
            .collect::<Vec<_>>(),
    )?))
}
fn verify_permits(
    failures: &[(String, String)],
    mappings: &[Mapping],
    permits: &[Permit],
) -> Result<()> {
    let mut covered = BTreeSet::new();
    for permit in permits {
        ensure!(
            failures.contains(&(permit.rule_id.clone(), permit.hunk_id.clone()))
                && covered.insert((permit.rule_id.clone(), permit.hunk_id.clone())),
            "constraint exception invented/duplicated violation"
        );
        ensure!(
            permit.reason.trim().len() >= 30 && !permit.criterion_ids.is_empty(),
            "constraint exception lacks requirement justification"
        );
        let mapping = mappings
            .iter()
            .find(|mapping| mapping.hunk_id == permit.hunk_id)
            .context("constraint exception hunk has no independent alignment")?;
        ensure!(
            permit
                .criterion_ids
                .iter()
                .all(|id| mapping.criterion_ids.contains(id)),
            "constraint exception names an unrelated owner criterion"
        );
    }
    ensure!(
        covered.len() == failures.len(),
        "promoted_constraint_failed: final authority did not justify every violation"
    );
    Ok(())
}
pub(super) fn assert_current(
    rules: &[Constraint],
    evidence: &[HunkEvidence],
    mappings: &[Mapping],
    head: &str,
    contract_hash: &str,
    policy_hash: &str,
    exception: Option<&Exception>,
) -> Result<()> {
    let failures = violations(rules, evidence)?;
    if failures.is_empty() {
        ensure!(exception.is_none(), "unnecessary constraint exception");
        return Ok(());
    }
    let exception =
        exception.context("promoted_constraint_failed: exact contract exception required")?;
    ensure!(
        exception.head == head
            && exception.contract_hash == contract_hash
            && exception.policy_hash == policy_hash
            && exception.input_hash == input_hash(evidence)?
            && !exception.native_session.is_empty()
            && !exception.provider.is_empty(),
        "constraint exception binding stale"
    );
    verify_permits(&failures, mappings, &exception.permits)
}
pub(super) async fn resolve(
    builder: &FallbackReasoner,
    root: &Path,
    prepared: &acceptance::PreparedAcceptance,
    workflow: &workflow::WorkflowGuard,
    rules: &[Constraint],
    evidence: &[HunkEvidence],
    mappings: &[Mapping],
    head: &str,
    policy_hash: &str,
) -> Result<Option<Exception>> {
    let failures = violations(rules, evidence)?;
    if failures.is_empty() {
        return Ok(None);
    }
    let contract_hash = prepared.baseline.registration.contract_hash();
    let input_hash = input_hash(evidence)?;
    let epoch = hash(&serde_json::to_vec(&(
        contract_hash,
        policy_hash,
        &input_hash,
    ))?);
    ensure!(
        workflow.admit_epoch_attempt("constraint-exception", &epoch, 2)?,
        "promoted_constraint_failed: unchanged conflict adjudication exhausted"
    );
    let affected = evidence
        .iter()
        .filter(|h| failures.iter().any(|(_, id)| id == &h.hunk.id))
        .map(|h| serde_json::json!({"hunk":h.hunk,"full_patch":h.patch}))
        .collect::<Vec<_>>();
    let prompt=format!("You have final authority over a conflict between a historical learned rule and the owner's immutable acceptance contract. The full candidate already passed independent executable verification and hunk-to-criterion alignment. A learned rule is subordinate to explicit owner intent, but cannot be waived for convenience or unrelated changes. Return ONLY JSON {{\"head\":\"{head}\",\"contract_hash\":\"{contract_hash}\",\"policy_hash\":\"{policy_hash}\",\"permits\":[{{\"rule_id\":\"violated rule\",\"hunk_id\":\"exact aligned hunk\",\"criterion_ids\":[\"existing aligned owner criterion\"],\"reason\":\"specific reason this owner requirement necessarily conflicts with this learned rule\"}}]}}. Omit any conflict you cannot justify; omitted conflicts require implementation repair. Never change the owner requirement, globally disable a rule, or grant a permit for future heads.\nContract:{}\nLearned rules:{}\nViolations:{}\nIndependent alignment:{}\nAffected full hunks:{}",serde_json::to_string(&prepared.proposal.contract)?,serde_json::to_string(rules)?,serde_json::to_string(&failures)?,serde_json::to_string(mappings)?,serde_json::to_string(&affected)?);
    ensure!(
        prompt.len() <= 1024 * 1024,
        "constraint authority evidence exceeds scoped request bound"
    );
    let (raw, provider, native_session) =
        learning::isolated(builder, root, workflow, &prompt).await?;
    let raw = raw
        .trim()
        .strip_prefix("```json\n")
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(raw.trim());
    let decision: Decision =
        serde_json::from_str(raw).context("constraint authority invalid structured response")?;
    ensure!(
        decision.head == head
            && decision.contract_hash == contract_hash
            && decision.policy_hash == policy_hash,
        "constraint authority binding mismatch"
    );
    let exception = Exception {
        head: head.into(),
        contract_hash: contract_hash.into(),
        policy_hash: policy_hash.into(),
        input_hash,
        provider,
        native_session,
        permits: decision.permits,
    };
    assert_current(
        rules,
        evidence,
        mappings,
        head,
        contract_hash,
        policy_hash,
        Some(&exception),
    )?;
    Ok(Some(exception))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exception_cannot_cover_another_hunk_or_criterion() {
        let failures = vec![("R1".into(), "hunk-1".into())];
        let mappings = vec![Mapping {
            hunk_id: "hunk-1".into(),
            criterion_ids: vec!["C1".into()],
            kind: Kind::Implementation,
            evidence: "specific behavior".into(),
        }];
        let mut permits = vec![Permit {
            rule_id: "R1".into(),
            hunk_id: "hunk-1".into(),
            criterion_ids: vec!["C1".into()],
            reason: "The explicit owner requirement needs this exact otherwise-forbidden change"
                .into(),
        }];
        assert!(verify_permits(&failures, &mappings, &permits).is_ok());
        permits[0].criterion_ids = vec!["other-owner-request".into()];
        assert!(verify_permits(&failures, &mappings, &permits).is_err());
        permits[0].criterion_ids = vec!["C1".into()];
        permits[0].hunk_id = "unrelated-hunk".into();
        assert!(verify_permits(&failures, &mappings, &permits).is_err());
        assert!(verify_permits(&failures, &mappings, &[]).is_err());
    }
}
