//! Original execution ownership within the already charged constructor budget.
//! No initial-repair budget, new model admission, or verifier approval is granted.
use super::*;

pub(super) fn reserve(_root:&Path,_workflow:&workflow::WorkflowGuard,_episode:&ConstructionEpisode,_measurement:&Measurement,_plan:&str,_body:&str)->Result<()> {Ok(())}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn constructed_original_plan_is_not_misclassified_as_unowned_initial_budget() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let measurement=super::super::staging::tests::owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
        let executor=verifier_executor_id().unwrap();
        let (plan,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap();
        verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        let contexts=verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard).unwrap();
        assert!(contexts.is_empty(),"constructor-owned measurement was assigned an unowned initial-repair budget: {contexts:?}");
        guard.finish().unwrap();
    }
}
