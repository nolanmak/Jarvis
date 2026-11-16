//! Unpaid S observation scheduling; the ordinary dispatcher retains admission.
use super::*;
use crate::autoship_workflow::WorkflowStore;

pub(super) fn reserve(path: &Path, repository: &str, now: i64) -> Result<bool> {
    let Some(reader)=WorkflowStore::open_read_only(path)? else {return Ok(false)};
    if !reader.has_due_activity_observation(Some(repository),now)? {return Ok(false);}
    drop(reader);
    Ok(WorkflowStore::open(path)?.reserve_due_activity_observations(repository,now)?>0)
}
