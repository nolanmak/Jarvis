//! Explicit relinquishment of a completed live metadata turn. No native
//! settlement, readiness, successor, body or executable capability is minted.
use super::*;

const KIND:&str="owner_publication_yielded";
const MAX_RECORD:usize=8192;
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Yielded {
    version:u32,workflow:String,request_id:String,owner:String,generation:i64,
    intent:Reference,completion_sequence:i64,completion_sha256:String,
    binding:Binding,at_ms:i64,
}

/// The event is the only journal, not a new permission/index. It must retain
/// exact immutable publication joins even after recovery changes the actor.
pub(super) fn validate_history(conn:&Connection,workflow:&str)->Result<usize> {
    let mut query=conn.prepare("SELECT seq,generation,substr(payload,1,8193),length(CAST(payload AS BLOB)),at_ms FROM workflow_event WHERE workflow=?1 AND kind=?2 ORDER BY seq LIMIT 1025")?;
    let rows=query.query_map(params![workflow,KIND],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len()<=1024,"owner publication yield history exceeds bound");
    let mut requests=std::collections::BTreeSet::new();let mut bytes=0usize;
    for (sequence,generation,raw,len,at) in rows {
        ensure!((1..=MAX_RECORD as i64).contains(&len) && raw.len() as i64==len,"owner publication yield record exceeds bound");
        let record:Yielded=serde_json::from_str(&raw)?;
        ensure!(record.version==1 && serde_json::to_string(&record)?==raw && record.workflow==workflow
            && record.generation==generation && record.at_ms==at && at>=0
            && requests.insert(record.request_id.clone()),"owner publication yield identity changed or duplicated");
        let (intent_raw,intent_sha):(String,String)=conn.query_row("SELECT substr(record,1,4227073),record_sha256 FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![workflow,record.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(record.intent==reference(&intent_raw) && record.intent.record_sha256==intent_sha,"owner publication yield intent changed");
        let intent:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&intent_raw)?;
        ensure!(intent.workflow()==workflow && intent.request()==record.request_id && intent.generation()==generation
            && intent.original_turn_metadata().0==record.owner && intent.recovery_binding()==&record.binding,
            "owner publication yield original actor changed");
        let (complete_seq,complete_sha,complete_raw):(i64,String,String)=conn.query_row("SELECT sequence,record_sha256,substr(record,1,32769) FROM workflow_owner_publication_completion WHERE workflow=?1 AND request_id=?2",params![workflow,record.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let completed:crate::self_improve::OwnerPublicationCompletion=serde_json::from_str(&complete_raw)?;
        ensure!(complete_seq==record.completion_sequence && complete_seq<sequence
            && complete_sha==record.completion_sha256 && reference(&complete_raw).record_sha256==complete_sha
            && completed.generation()==generation && completed.at_ms()<=at,
            "owner publication yield completion changed");
        bytes=bytes.checked_add(raw.len()).context("owner publication yield history size overflow")?;
    }
    Ok(bytes)
}

impl WorkflowStore {
    /// Release only the current opaque live metadata actor. The original input,
    /// native registrations/effects, pause/checkpoint, spend and shipping history
    /// stay intact. Recovery must acquire its own fresh authorization and claim.
    /// This runs outside a native visitor: ending a lease is not teardown proof.
    pub(crate) fn yield_completed_owner_publication(&mut self,metadata:&LiveOwnerTurn,
        publication:&crate::self_improve::OwnerPublicationDraft<'_>,now:i64,
    )->Result<()> {
        let turn=&metadata.turn;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,&turn.lease,&turn.binding,now)?;
        // This is unpaid relinquishment, so newer pause/control state is not
        // interpreted as permission to execute and must not be overwritten.
        owner_revision::require_preparation(&tx,turn,&metadata.preparation)?;
        publication.require_metadata_record(&tx,turn)?;
        let pending=owner_inbox::require_pending(&tx,&turn.lease.workflow,&turn.request_id)?;
        ensure!(pending.body==turn.body,"owner publication yield original input changed");
        let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2 AND owner=?3 AND lease_generation=?4 AND pause_revision=?5 AND binding=?6 AND state='claimed')",
            params![turn.lease.workflow,turn.request_id,turn.lease.owner,turn.lease.generation,turn.pause_revision,serde_json::to_string(&turn.binding)?],|r|r.get(0))?;
        ensure!(live,"owner publication yield original turn changed");
        super::validate_history(&tx,&turn.lease.workflow)?;
        let raw:String=tx.query_row("SELECT record FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![turn.lease.workflow,turn.request_id],|r|r.get(0))?;
        require(&tx,&turn.request_id,&metadata.intent,&raw)?;
        let intent:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
        let completed=completion(&tx,&turn.lease.workflow,&turn.request_id,&intent,&raw)?
            .context("owner publication yield requires completed publication")?;
        intent.validate_completed_canonical_files(&completed)?;
        let checkpoint:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&turn.lease.workflow],|r|r.get(0))?;
        ensure!(!matches!(serde_json::from_str::<Checkpoint>(&checkpoint)?.stage,Stage::Merged|Stage::Retired),
            "owner publication yield cannot resurrect terminal workflow");
        let (completion_sequence,completion_sha256)=tx.query_row("SELECT sequence,record_sha256 FROM workflow_owner_publication_completion WHERE workflow=?1 AND request_id=?2",params![turn.lease.workflow,turn.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let record=Yielded {version:1,workflow:turn.lease.workflow.clone(),request_id:turn.request_id.clone(),
            owner:turn.lease.owner.clone(),generation:turn.lease.generation,intent:metadata.intent.clone(),
            completion_sequence,completion_sha256,binding:turn.binding.clone(),at_ms:now};
        let encoded=serde_json::to_string(&record)?;
        ensure!(encoded.len()<=MAX_RECORD,"owner publication yield exceeds bound");
        require_binding(&tx,&turn.lease,&turn.binding,chrono::Utc::now().timestamp_millis())?;
        event(&tx,&turn.lease.workflow,turn.lease.generation,KIND,&encoded,now)?;
        ensure!(tx.execute("UPDATE workflow SET owner=NULL,lease_until=0 WHERE id=?1 AND owner=?2 AND generation=?3",
            params![turn.lease.workflow,turn.lease.owner,turn.lease.generation])?==1,"owner publication yield lost current actor");
        super::validate_history(&tx,&turn.lease.workflow)?;
        super::super::progress::project(&tx,&turn.lease.workflow,KIND,now)?;
        tx.commit()?;Ok(())
    }
}

#[cfg(test)]
pub(super) fn fixture_encoding(raw:&str)->Result<String> {
    Ok(serde_json::to_string(&serde_json::from_str::<Yielded>(raw)?)?)
}
