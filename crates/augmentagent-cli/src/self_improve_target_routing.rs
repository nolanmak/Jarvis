//! Route an already-authorized resolved target into the existing execution lanes.
//! This changes scheduling only: preflight, workflow ownership and budgets remain
//! enforced by run_selected and its normal production callees.
use super::*;

pub(super) async fn dispatch<F, Fut>(selected: Resolved, run: F) -> Result<RunReport>
where
    F: FnOnce(Resolved) -> Fut,
    Fut: std::future::Future<Output = Result<RunReport>>,
{
    let lane = match &selected {
        Resolved::Issue(_) => Lane::Build,
        Resolved::Pr(_) => Lane::Resume,
    };
    COMMAND_LANE.scope(lane, run(selected)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(source_issue: Option<u64>) -> Resolved {
        Resolved::Pr(adoption::AdoptedPr {
            repository: "owner/repo".into(), pr: 42, source_issue,
            branch: "candidate".into(), head: "reviewed-head".into(), base: "main".into(),
            author: "owner".into(), title: "Ordinary fix".into(), body: String::new(),
            dependency: adoption::Dependency::Main,
        })
    }
    fn issue() -> Resolved {
        Resolved::Issue(Issue {number:7,title:"Ordinary issue".into(),body:String::new(),
            author:"owner".into(),author_trusted:true,research_filed:false})
    }
    fn lock_path(root: &Path, lane: Lane) -> PathBuf {
        root.join(format!("self-improve{}.lock",lane.lock_suffix()))
    }

    #[tokio::test]
    async fn resolved_pr_can_enter_resume_while_a_builder_holds_its_lock() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path();
        let _builder = RunLock::try_acquire(&lock_path(root.path(),Lane::Build)).unwrap().unwrap();
        for source in [None,Some(7)] {
            dispatch(pr(source), |selected| async move {
                assert!(matches!(selected,Resolved::Pr(_)));
                assert_eq!(lane_from_env(),Lane::Resume,"resolved PR was routed back through the busy builder lane");
                let path=lock_path(root_path,lane_from_env());
                let _resume=RunLock::try_acquire(&path)?.expect("ready PR blocked by unrelated builder");
                assert!(RunLock::try_acquire(&path)?.is_none(),"two resume dispatches shared custody");
                assert_ne!(lane_from_env().worktree_name(),Lane::Build.worktree_name());
                Ok(RunReport::idle())
            }).await.unwrap();
        }
    }

    #[tokio::test]
    async fn issue_dispatch_keeps_builder_exclusion_and_restores_outer_lane() {
        COMMAND_LANE.scope(Lane::Resume,async {
            let root=tempfile::tempdir().unwrap();
            let _builder=RunLock::try_acquire(&lock_path(root.path(),Lane::Build)).unwrap().unwrap();
            let root_path=root.path();
            dispatch(issue(),|selected|async move {
                assert!(matches!(selected,Resolved::Issue(_)));
                assert_eq!(lane_from_env(),Lane::Build,"issue did not use the builder lane");
                assert!(RunLock::try_acquire(&lock_path(root_path,lane_from_env()))?.is_none(),"issue bypassed builder exclusion");
                Ok(RunReport::idle())
            }).await.unwrap();
            assert_eq!(lane_from_env(),Lane::Resume);
        }).await;
    }
}
