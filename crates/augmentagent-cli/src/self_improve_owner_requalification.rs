//! Give owner recoveries deterministic, bounded turns without minting authority.
use super::*;
use crate::autoship_workflow::{owner_requalification, WorkflowStore};

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
        store.owner_requalification_turns(&crate::autoship_workflow::workflow_id(
            repository, number,
        )?)
    })
}

fn prioritize_at(
    candidates: Vec<RestIssue>,
    pending: &std::collections::BTreeSet<u64>,
    now: i64,
    mut selected: impl FnMut(u64) -> Result<Option<(i64, u32)>>,
) -> Result<Vec<RestIssue>> {
    let mut ranked = vec![];
    for issue in candidates {
        let owner = pending.contains(&issue.number);
        let turn = if owner { selected(issue.number)? } else { None };
        if turn.is_some_and(|(last, n)| {
            now < last.saturating_add(owner_requalification::cooldown_ms(n))
        }) {
            continue;
        }
        // Existing lease/continuity filters precede this stable sort. Never-tried
        // owner recoveries precede retries; preserve peer fairness for equal age.
        // #2317: an owner hold that keeps returning unchanged loses its
        // priority and rotates behind peers, like #2280's repeated claims.
        let last = turn.map_or(0, |(last, _)| last);
        let priority = owner && !turn.is_some_and(|(_, n)| owner_requalification::yields(n));
        ranked.push(((!priority, last), issue));
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
        let selected = |number| Ok((number == 7).then_some((10, 1)));
        assert_eq!(
            prioritize_at(rows(), &pending, 11, selected)
                .unwrap()
                .into_iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![6, 9, 8]
        );
        assert_eq!(
            prioritize_at(rows(), &pending, 10 + owner_requalification::RETRY_MS, selected)
                .unwrap()
                .into_iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![6, 7, 9, 8]
        );
    }
    #[test]
    fn owner_recovery_without_progress_yields_to_peers_with_backoff() {
        // #2317: #1644 returned to the same evidence_unknown hold on every
        // owner turn and was re-sorted first every cooldown.
        let pending = [7, 6].into_iter().collect();
        let order = |now, turns| {
            prioritize_at(rows(), &pending, now, |number| {
                Ok((number == 7).then_some((10, turns)))
            })
            .unwrap()
            .into_iter()
            .map(|i| i.number)
            .collect::<Vec<_>>()
        };
        let retry = owner_requalification::RETRY_MS;
        // Fewer than three unchanged turns keep owner priority.
        assert_eq!(order(10 + retry, 2), vec![6, 7, 9, 8]);
        // The third unchanged turn doubles the cooldown...
        assert_eq!(order(10 + retry, 3), vec![6, 9, 8]);
        // ...and once due it rotates behind peers instead of ahead of them.
        assert_eq!(order(10 + 2 * retry, 3), vec![6, 9, 8, 7]);
        // Each further unchanged turn doubles the wait again.
        assert_eq!(order(10 + 2 * retry, 4), vec![6, 9, 8]);
        assert_eq!(order(10 + 4 * retry, 4), vec![6, 9, 8, 7]);
    }
}
