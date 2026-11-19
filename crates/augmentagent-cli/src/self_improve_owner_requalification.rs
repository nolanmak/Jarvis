//! Give owner recoveries deterministic, bounded turns without minting authority.
use super::*;
use crate::autoship_workflow::{owner_requalification::RETRY_MS, WorkflowStore};

pub(super) fn prioritize(
    state: &Path,
    candidates: Vec<RestIssue>,
    pending: &std::collections::BTreeSet<u64>,
    repository: &str,
    now: i64,
) -> Result<Vec<RestIssue>> {
    if pending.is_empty() {
        return Ok(candidates);
    }
    let store = WorkflowStore::open_read_only(state)?
        .context("owner recovery scheduling history disappeared")?;
    prioritize_at(candidates, pending, now, |number| {
        store.owner_requalification_selected_at(&crate::autoship_workflow::workflow_id(
            repository, number,
        )?)
    })
}

fn prioritize_at(
    candidates: Vec<RestIssue>,
    pending: &std::collections::BTreeSet<u64>,
    now: i64,
    mut selected: impl FnMut(u64) -> Result<Option<i64>>,
) -> Result<Vec<RestIssue>> {
    let mut ranked = vec![];
    for issue in candidates {
        let owner = pending.contains(&issue.number);
        let last = if owner { selected(issue.number)? } else { None };
        if last.is_some_and(|last| now < last.saturating_add(RETRY_MS)) {
            continue;
        }
        // Existing lease/continuity filters precede this stable sort. Never-tried
        // owner recoveries precede retries; preserve peer fairness for equal age.
        ranked.push(((!owner, last.unwrap_or(0)), issue));
    }
    ranked.sort_by_key(|(key, _)| *key);
    Ok(ranked.into_iter().map(|(_, issue)| issue).collect())
}

pub(super) fn reserve(state: &Path, repository: &str, number: u64, now: i64) -> Result<bool> {
    WorkflowStore::open_read_only(state)?
        .context("owner recovery scheduling history disappeared")?;
    WorkflowStore::open(state)?.reserve_owner_requalification(
        &crate::autoship_workflow::workflow_id(repository, number)?,
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rows() -> Vec<RestIssue> {
        [9, 7, 8, 6]
            .map(|number| RestIssue {
                number,
                title: "Synthetic widget".into(),
                body: "Synthetic request".into(),
                author: "fixture-owner".into(),
                association: "OWNER".into(),
                research_filed: false,
            })
            .into()
    }
    #[test]
    fn owner_recovery_precedes_other_recovery_and_preserves_peer_fairness() {
        let pending = [7, 6].into_iter().collect();
        assert_eq!(
            prioritize_at(rows(), &pending, 10, |_| Ok(None))
                .unwrap()
                .into_iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![7, 6, 9, 8]
        );
    }
    #[test]
    fn unsuccessful_preclaim_turn_cools_down_then_retries_behind_untouched_owners() {
        let pending = [7, 6].into_iter().collect();
        let selected = |number| Ok((number == 7).then_some(10));
        assert_eq!(
            prioritize_at(rows(), &pending, 11, selected)
                .unwrap()
                .into_iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![6, 9, 8]
        );
        assert_eq!(
            prioritize_at(rows(), &pending, 10 + RETRY_MS, selected)
                .unwrap()
                .into_iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![6, 7, 9, 8]
        );
    }
}
