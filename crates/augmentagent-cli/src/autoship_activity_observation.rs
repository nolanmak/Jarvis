//! Bounded unpaid wake reservations over S's persisted observation deadlines.
use super::*;

const BATCH: usize = 16;
const INTERVAL_MS: i64 = 300_000;

fn due(conn: &Connection, repository: Option<&str>, now: i64, limit: usize)
    -> Result<Vec<(String,i64,super::activity::Activity)>> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_activity')", [], |r|r.get(0))?;
    if !exists {return Ok(Vec::new());}
    let repository = repository.map(super::submission::normalized_repository).transpose()?;
    let controls: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_control')", [], |r|r.get(0))?;
    let mut sql=String::from("SELECT w.id,w.generation,w.checkpoint,a.payload FROM workflow w JOIN workflow_activity a ON a.workflow=w.id
        WHERE w.lease_until<=?2 AND (?1 IS NULL OR instr(w.id,?1||'#')=1 OR instr(w.id,?1||'/pr/')=1)");
    if controls {sql.push_str(" AND NOT EXISTS(SELECT 1 FROM workflow_owner_control c WHERE c.workflow=w.id AND c.paused=1)");}
    sql.push_str(" ORDER BY w.id");
    let mut query=conn.prepare(&sql)?;
    let rows=query.query_map(params![repository,now],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2),r.get::<_,String>(3))))?;
    let mut selected=Vec::new();
    for row in rows {
        let (id,generation,checkpoint,activity)=row?;
        // Unknown individual projections are not invented work or a reason to
        // suppress a healthy peer. Shared query/storage errors still propagate.
        let (Ok(checkpoint),Ok(activity))=(checkpoint,activity) else {continue};
        let (Ok(checkpoint),Ok(activity))=(serde_json::from_str::<Checkpoint>(&checkpoint),serde_json::from_str::<super::activity::Activity>(&activity)) else {continue};
        if matches!(checkpoint.stage,Stage::Merged|Stage::Retired) || activity.schema_version!=1
            || activity.inactivity_deadline_at.is_some()
            || !activity.next_observation_at.is_some_and(|at|at<=now) {continue;}
        selected.push((id,generation,activity));
        if selected.len()>=limit {break;}
    }
    Ok(selected)
}
impl WorkflowStore {
    /// A read-only wake hint over known native activity, not a claim or a
    /// complete-inventory/healthy-idle assertion for missing legacy state.
    pub(crate) fn has_due_activity_observation(&self, repository: Option<&str>, now: i64) -> Result<bool> {
        Ok(!due(&self.conn,repository,now,1)?.is_empty())
    }
    /// Re-arm at most one bounded batch before waking the ordinary dispatcher.
    /// No dependency has been proved healthy and no workflow custody is changed.
    pub(crate) fn reserve_due_activity_observations(&mut self, repository: &str, now: i64) -> Result<usize> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let selected=due(&tx,Some(repository),now,BATCH)?;
        for (id,generation,mut activity) in selected.iter().cloned() {
            activity.next_observation_at=Some(now.saturating_add(INTERVAL_MS));
            tx.execute("UPDATE workflow_activity SET payload=?2 WHERE workflow=?1",
                params![id,serde_json::to_string(&activity)?])?;
            event(&tx,&id,generation,"activity_observation_scheduled","{}",now)?;
        }
        tx.commit()?;
        Ok(selected.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    const NOW: i64 = 1_000_000;
    const FIVE_MINUTES: i64 = 300_000;
    fn setup(retain_owner: bool, ttl: i64) -> (tempfile::TempDir, WorkflowStore, Lease) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let binding = Binding {head:"candidate".into(), contract:"owner-criteria".into(), evidence_hash:"evidence".into()};
        store.register("owner/repo#7", "candidate", binding, NOW).unwrap();
        let lease = store.claim("owner/repo#7", "controller", NOW, ttl).unwrap().unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage = Stage::Scope;
        checkpoint.next = NextAction {reason:"acceptance_contract_invalid".into(),
            action:"await_acceptance_input_change".into(), eligible_at_ms:0};
        store.checkpoint(&lease, &checkpoint, NOW).unwrap();
        if !retain_owner {store.release(&lease, NOW).unwrap();}
        (root,store,lease)
    }
    fn unchanged(before: &Snapshot, after: &Snapshot) {
        assert_eq!(after.checkpoint, before.checkpoint);
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.lease_until_ms, before.lease_until_ms);
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.model_calls, before.model_calls);
        assert_eq!(after.tokens, before.tokens);
        assert_eq!(after.wall_ms, before.wall_ms);
        assert_eq!(after.progress_at_ms, before.progress_at_ms);
    }
    #[test]
    fn due_wait_rearms_once_and_survives_read_only_reopen_without_claiming() {
        let (root, mut store, lease) = setup(false, 60_000);
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(!store.has_due_activity_observation(None, NOW+FIVE_MINUTES-1).unwrap());
        assert!(store.has_due_activity_observation(None, NOW+FIVE_MINUTES).unwrap());
        assert!(!store.has_due_activity_observation(Some("foreign/repo"), NOW+FIVE_MINUTES).unwrap());
        assert_eq!(store.reserve_due_activity_observations("owner/repo", NOW+FIVE_MINUTES).unwrap(),1);
        unchanged(&before,&store.snapshot(&lease.workflow).unwrap().unwrap());
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(activity.next_observation_at,Some(NOW+2*FIVE_MINUTES));
        assert_eq!(activity.last_meaningful_progress_at,0);
        assert!(activity.inactivity_deadline_at.is_none());
        assert_eq!(store.reserve_due_activity_observations("owner/repo", NOW+FIVE_MINUTES+1).unwrap(),0);
        drop(store);
        let reader = WorkflowStore::open_read_only(&root.path().join("workflow.db")).unwrap().unwrap();
        assert!(!reader.has_due_activity_observation(Some("owner/repo"),NOW+FIVE_MINUTES+1).unwrap());
        assert!(reader.has_due_activity_observation(Some("owner/repo"),NOW+2*FIVE_MINUTES).unwrap());
        unchanged(&before,&reader.snapshot(&lease.workflow).unwrap().unwrap());
    }
    #[test]
    fn expired_owner_custody_is_not_released_or_renewed_by_unpaid_observation() {
        let (_root, mut store, lease) = setup(true,60_000);
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(store.reserve_due_activity_observations("owner/repo", NOW+FIVE_MINUTES).unwrap(),1);
        unchanged(&before,&store.snapshot(&lease.workflow).unwrap().unwrap());
        let owner:Option<String>=store.conn.query_row("SELECT owner FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0)).unwrap();
        assert_eq!(owner.as_deref(),Some("controller"));
        assert!(store.validate_lease(&lease,&before.checkpoint.binding,NOW+FIVE_MINUTES).is_err());
    }
    #[test]
    fn active_and_owner_paused_waits_do_not_wake_the_builder() {
        let (_root, mut active, _) = setup(true,3_600_000);
        assert!(!active.has_due_activity_observation(None,NOW+2*FIVE_MINUTES).unwrap());
        assert_eq!(active.reserve_due_activity_observations("owner/repo",NOW+2*FIVE_MINUTES).unwrap(),0);
        let (_root, mut paused, lease) = setup(false,60_000);
        paused.set_owner_pause(&lease.workflow,"github:owner",0,true,NOW+20).unwrap();
        assert!(!paused.has_due_activity_observation(None,NOW+2*FIVE_MINUTES).unwrap());
        assert_eq!(paused.reserve_due_activity_observations("owner/repo",NOW+2*FIVE_MINUTES).unwrap(),0);
    }
    #[test]
    fn missing_legacy_activity_is_not_a_due_work_invention() {
        let root=tempfile::tempdir_in("/tmp").unwrap();let path=root.path().join("legacy.db");
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE legacy_fixture(id INTEGER PRIMARY KEY)").unwrap();
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600)).unwrap();
        let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert!(!store.has_due_activity_observation(None,NOW).unwrap());
    }
}
