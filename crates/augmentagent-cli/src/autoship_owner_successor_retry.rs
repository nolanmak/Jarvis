//! Setup-only scheduling journal. Neither retry time nor saved history renews
//! the live setup claim, authorizes input, or retires native reservations.
use super::*;
use crate::autoship_workflow::owner_publication::OwnerPublicationNativeFacts;
use augmentagent_channel_core::native_vm::ownership::NoPendingPreparationFailure;

const KIND:&str="owner_successor_native_preparation_retry";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    version:u32,setup:RecordRef,previous:Option<RecordRef>,facts_sha256:String,
    fingerprint:String,reason:String,deferred:bool,failures:u32,
    failed_at_ms:i64,eligible_at_ms:i64,
}
fn hash_valid(s:&str)->bool {s.len()==64 && s.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'))}
fn delay(n:u32)->i64 {crate::autoship_workflow::capacity_retry::delay(n,true)}
/// workflow_event is the sole durable journal (no separately mutable index).
/// Validate every bounded predecessor before using its scheduling observation.
fn history(conn:&Connection,setup:&SuccessorSetup,now:i64)->Result<Option<(Retry,RecordRef)>> {
    let mut q=conn.prepare("SELECT seq,generation,substr(payload,1,8193),length(CAST(payload AS BLOB)),at_ms FROM workflow_event WHERE workflow=?1 AND kind=?2 ORDER BY seq LIMIT 1025")?;
    let rows=q.query_map(params![setup.workflow_id(),KIND],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len()<=1024,"successor retry history exceeds bound");
    let mut previous:Option<(Retry,RecordRef)>=None;
    for (seq,generation,raw,len,at) in rows {
        ensure!((1..=8192).contains(&len) && raw.len() as i64==len,"successor retry record exceeds bound");
        let retry:Retry=serde_json::from_str(&raw)?;
        ensure!(serde_json::to_string(&retry)?==raw && retry.version==1 && retry.setup==setup.reference
            && generation==setup.generation() && seq>setup.reference.sequence
            && retry.failed_at_ms==at && at>=setup.record.at_ms && at<setup.record.lease_until && at<=now
            && retry.previous.as_ref()==previous.as_ref().map(|(_,reference)|reference)
            && hash_valid(&retry.facts_sha256) && hash_valid(&retry.fingerprint)
            && !retry.reason.is_empty() && retry.reason.len()<=128 && retry.reason.bytes().all(|b|b.is_ascii_graphic())
            && retry.failures>0 && at.checked_add(delay(retry.failures))==Some(retry.eligible_at_ms),
            "successor retry journal changed");
        if let Some((prior,_))=&previous {
            let unchanged=prior.facts_sha256==retry.facts_sha256 && prior.fingerprint==retry.fingerprint;
            ensure!(at>=prior.failed_at_ms && retry.failures==if unchanged {prior.failures.checked_add(1).context("successor retry count overflow")?}else{1}
                && (!unchanged || at>=prior.eligible_at_ms),"successor retry predecessor changed");
        }else{ensure!(retry.failures==1,"successor retry first observation missing");}
        previous=Some((retry,RecordRef {sequence:seq,sha256:digest(&raw)}));
    }
    Ok(previous)
}
fn validate(conn:&Connection,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,facts:&OwnerPublicationNativeFacts,now:i64)->Result<String> {
    ensure!(transfer::selected(conn,setup,authorization,now)?.is_none(),"successor retry cannot hide a pending transfer");
    let remote:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",[setup.workflow_id()],|r|r.get(0))?;
    ensure!(!remote,"successor retry has unresolved remote operation");
    native_pending::require_clear_abort_barriers(conn,setup.workflow_id())?;
    let actual=native::facts(conn,setup,now)?.fingerprint()?;
    ensure!(actual==facts.fingerprint()?,"successor retry native facts changed");
    Ok(actual)
}
impl WorkflowStore {
    pub(crate) fn owner_successor_native_retry_due(&mut self,setup:&SuccessorSetup,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,facts:&OwnerPublicationNativeFacts,now:i64,
    )->Result<Option<i64>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let actual=validate(&tx,setup,authorization,facts,now)?;
        let due=history(&tx,setup,now)?.filter(|(r,_)|r.facts_sha256==actual && r.eligible_at_ms>now).map(|(r,_)|r.eligible_at_ms);
        tx.commit()?;Ok(due)
    }
    pub(crate) fn defer_owner_successor_native_preparation(&mut self,setup:&SuccessorSetup,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,facts:&OwnerPublicationNativeFacts,
        failure:&NoPendingPreparationFailure,now:i64,
    )->Result<i64> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual=validate(&tx,setup,authorization,facts,now)?;
        ensure!(failure.workflow_id()==setup.workflow_id() && failure.workflow_binding()==&serde_json::to_value(&setup.record.checkpoint.binding)?,"successor retry failure scope changed");
        let prior=history(&tx,setup,now)?;
        let unchanged=prior.as_ref().filter(|(r,_)|r.facts_sha256==actual && r.fingerprint==failure.fingerprint());
        if let Some((r,_))=unchanged.filter(|(r,_)|r.eligible_at_ms>now) {let due=r.eligible_at_ms;tx.commit()?;return Ok(due);}
        let failures=unchanged.map(|(r,_)|r.failures.checked_add(1).context("successor retry count overflow")).transpose()?.unwrap_or(1);
        let due=now.checked_add(delay(failures)).context("successor retry time overflow")?;
        let retry=Retry {version:1,setup:setup.reference.clone(),previous:prior.map(|(_,reference)|reference),facts_sha256:actual,
            fingerprint:failure.fingerprint().into(),reason:failure.reason().code().into(),deferred:failure.is_deferred(),failures,failed_at_ms:now,eligible_at_ms:due};
        let raw=serde_json::to_string(&retry)?;ensure!(raw.len()<=8192,"successor retry record exceeds bound");
        event(&tx,setup.workflow_id(),setup.generation(),KIND,&raw,now)?;
        ensure!(history(&tx,setup,now)?.is_some_and(|(_,reference)|reference.sha256==digest(&raw)),"successor retry append changed");
        tx.commit()?;Ok(due)
    }
}
