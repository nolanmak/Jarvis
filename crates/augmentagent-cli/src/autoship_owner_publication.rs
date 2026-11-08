//! Immutable private intent/completion history; never coding authority.
use super::*;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {version:u32,record_sha256:String}
pub(super) fn reference(raw:&str)->Reference {Reference {version:1,record_sha256:format!("{:x}",Sha256::digest(raw.as_bytes()))}}
pub(super) fn persist(tx:&Transaction<'_>,raw:&str,now:i64)->Result<Reference> {
    ensure!(raw.len()<=4*1024*1024+32*1024,"owner publication intent exceeds bound");
    let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(raw)?;
    ensure!(serde_json::to_string(&record)?==raw,"owner publication intent encoding changed");record.validate_history(tx)?;
    let reference=reference(raw);
    event(tx,record.workflow(),record.generation(),"owner_publication_intended",&serde_json::json!({"version":1,"request_id":record.request(),"record_sha256":reference.record_sha256}).to_string(),now)?;
    let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[record.workflow()],|r|r.get(0))?;
    tx.execute("INSERT INTO workflow_owner_publication_intent VALUES(?1,?2,?3,?4,?5)",params![record.request(),record.workflow(),sequence,reference.record_sha256,raw])?;
    tx.execute("INSERT INTO workflow_owner_publication_pending VALUES(?1,?2)",params![record.workflow(),record.request()])?;Ok(reference)
}
pub(super) fn require(conn:&Connection,request:&str,expected:&Reference,raw:&str)->Result<()> {
    let actual:(String,String)=conn.query_row("SELECT record_sha256,record FROM workflow_owner_publication_intent WHERE request_id=?1",[request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(expected.version==1 && expected==&reference(raw) && actual.0==expected.record_sha256 && actual.1==raw,"owner publication original intent changed");Ok(())
}
/// Distinct transaction-scoped owner writer capability. A generic acceptance
/// metadata visitor cannot construct it or bypass the original intent join.
pub(crate) struct OwnerPublicationCommitMetadata<'a> {_transaction:&'a Connection,workflow:&'a str,request:&'a str,intent_sha256:String,authority:crate::self_improve::OwnerPublicationAuthority,current:&'a dyn Fn()->Result<()>}
impl OwnerPublicationCommitMetadata<'_> {
    pub(crate) fn authority(&self)->&crate::self_improve::OwnerPublicationAuthority {&self.authority}
    pub(crate) fn require(&self,workflow:&str,request:&str,intent_sha256:&str)->Result<()> {
        ensure!(self.workflow==workflow && self.request==request && self.intent_sha256==intent_sha256,"owner publication commit authority differs from original intent");(self.current)()
    }
}
/// Read-only transaction-bound lineage context. It never grants a model,
/// native, scope mutation, body or publication capability.
pub(crate) struct OwnerPublicationReadContext<'a> {conn:&'a Connection,workflow:&'a str,records:std::cell::RefCell<Option<std::collections::BTreeMap<String,(crate::self_improve::OwnerPublicationRecord,Option<crate::self_improve::OwnerPublicationCompletion>)>>>}
impl AcceptanceMetadata<'_> {
    pub(crate) fn publication_reader(&self)->OwnerPublicationReadContext<'_> {
        OwnerPublicationReadContext {conn:self._transaction,workflow:self.workflow,records:std::cell::RefCell::new(None)}
    }
}
impl OwnerPublicationReadContext<'_> {
    fn load(&self)->Result<()> {
        if self.records.borrow().is_none() {
            validate_history(self.conn,self.workflow)?;
            let mut records=std::collections::BTreeMap::new();let mut used_records=0usize;
            let mut query=self.conn.prepare("SELECT request_id,substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
            let mut rows=query.query([self.workflow])?;let mut count=0;
            while let Some(row)=rows.next()? {
                count+=1;let request:String=row.get(0)?;let raw:String=row.get(1)?;let len:i64=row.get(2)?;
                used_records=used_records.checked_add(raw.len()).context("owner lineage journal size overflow")?;
                ensure!(count<=1024 && used_records<=128*1024*1024 && (1..=4*1024*1024+32*1024).contains(&len) && raw.len() as i64==len,"owner lineage intent exceeds bound");
                let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
                let completed=completion(self.conn,self.workflow,&request,&record,&raw)?;
                ensure!(records.insert(record.canonical_contract().to_owned(),(record,completed)).is_none(),"canonical owner registration has ambiguous original publication intents");
            }
            *self.records.borrow_mut()=Some(records);
        }
        Ok(())
    }
    pub(crate) fn require_manifest_identity(&self,contract:&str,manifest:&str)->Result<()> {
        self.load()?;
        if let Some((record,_))=self.records.borrow().as_ref().and_then(|records|records.get(contract)) {
            ensure!(record.canonical_manifest()==manifest,"known owner registration changed its original canonical identity");
        }
        Ok(())
    }
    pub(crate) fn ancestor(&self,control:&Path,source:crate::autoship_contract::AcceptanceSource,manifest:&str,used:&mut usize)->Result<Option<String>> {
        self.load()?;
        let value:serde_json::Value=serde_json::from_str(manifest)?;
        let contract=value["contract_hash"].as_str().context("owner canonical contract missing")?;
        let records=self.records.borrow();
        let (record,completed)=records.as_ref().and_then(|records|records.get(contract)).context("canonical owner manifest has no original publication intent")?;
        ensure!(record.canonical_manifest()==manifest,"owner canonical manifest differs from original publication");
        record.read_canonical_ancestor(control,source,manifest,completed.as_ref().context("canonical owner publication is not completed")?,used)
    }
}
pub(super) fn completion(conn:&Connection,workflow:&str,request:&str,intent:&crate::self_improve::OwnerPublicationRecord,raw_intent:&str)->Result<Option<crate::self_improve::OwnerPublicationCompletion>> {
    let row:Option<(i64,String,String,i64,i64)>=conn.query_row("SELECT sequence,substr(record_sha256,1,65),substr(record,1,32769),length(CAST(record AS BLOB)),length(CAST(record_sha256 AS BLOB)) FROM workflow_owner_publication_completion WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((sequence,sha,raw,len,sha_len))=row else {return Ok(None)};
    ensure!((1..=32768).contains(&len) && raw.len() as i64==len && sha_len==64 && sha.len()==64,"owner publication completion exceeds bound");
    let completed:crate::self_improve::OwnerPublicationCompletion=serde_json::from_str(&raw)?;
    ensure!(serde_json::to_string(&completed)?==raw && reference(&raw).record_sha256==sha,"owner publication immutable completion changed");
    completed.validate(conn,intent,raw_intent)?;
    completed.validate_event_position(conn,intent,sequence)?;
    let (kind,generation,payload,at,payload_len):(String,i64,String,i64,i64)=conn.query_row("SELECT kind,generation,substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let (before,before_at):(i64,i64)=conn.query_row("SELECT i.sequence,e.at_ms FROM workflow_owner_publication_intent i JOIN workflow_event e ON e.seq=i.sequence AND e.workflow=i.workflow WHERE i.workflow=?1 AND i.request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!((1..=4096).contains(&payload_len) && payload.len() as i64==payload_len && sequence>before && at>=before_at && at==completed.at_ms() && completed.recovery_sequence().is_none_or(|claim|claim<sequence) && kind=="owner_publication_completed" && generation==completed.generation()
        && serde_json::from_str::<serde_json::Value>(&payload)?==serde_json::json!({"version":1,"request_id":request,"record_sha256":sha,"intent_sha256":completed.intent_sha256()}),
        "owner publication completion event changed");Ok(Some(completed))
}
pub(super) fn validate_history(conn:&Connection,workflow:&str)->Result<()> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_intended'",[workflow],|r|r.get(0))?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_publication_intent')",[],|r|r.get(0))?;
    if !exists {
        let completed_events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_completed'",[workflow],|r|r.get(0))?;
        ensure!(events==0 && completed_events==0,"owner publication index missing");return Ok(())
    }
    let mut q=conn.prepare("SELECT request_id,sequence,record_sha256,record FROM workflow_owner_publication_intent WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let mut count=0;let mut bytes=0usize;let mut completed_count=0;
    for row in q.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
        let (request,sequence,sha,raw)=row?;count+=1;bytes=bytes.checked_add(raw.len()).context("owner publication history size overflow")?;
        ensure!(count<=1024 && raw.len()<=4*1024*1024+32*1024 && bytes<=128*1024*1024,"owner publication history exceeds bound");
        let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
        ensure!(record.workflow()==workflow && record.request()==request && serde_json::to_string(&record)?==raw && reference(&raw).record_sha256==sha,"owner publication record changed");record.validate_history(conn)?;
        let event:(String,i64,String)=conn.query_row("SELECT kind,generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(event.0=="owner_publication_intended" && event.1==record.generation() && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"version":1,"request_id":request,"record_sha256":sha}),"owner publication event changed");
        let mut activation=conn.prepare("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind IN ('owner_acceptance_registered','owner_contract_revised') AND json_extract(payload,'$.request_id')=?2 LIMIT 2")?;
        let activations=activation.query_map(params![workflow,request],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(activations.len()==1 && serde_json::from_str::<serde_json::Value>(&activations[0])?["publication"]==serde_json::to_value(reference(&raw))?,"owner publication lost its activation reference");
        let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_publication_pending WHERE workflow=?1 AND request_id=?2)",params![workflow,request],|r|r.get(0))?;
        let completed=completion(conn,workflow,&request,&record,&raw)?;
        ensure!(pending!=completed.is_some(),"owner publication must be pending or completed with exact history");
        if let Some(completed)=completed {completed_count+=1;bytes=bytes.checked_add(serde_json::to_vec(&completed)?.len()).and_then(|b|b.checked_add(completed.native_bytes())).context("owner publication completion history size overflow")?;
            ensure!(bytes<=128*1024*1024,"owner publication completed history exceeds bound");}
    }
    ensure!(count==events,"owner publication history incomplete");
    let pending:i64=conn.query_row("SELECT count(*) FROM workflow_owner_publication_pending WHERE workflow=?1",[workflow],|r|r.get(0))?;
    let completed:i64=conn.query_row("SELECT count(*) FROM workflow_owner_publication_completion WHERE workflow=?1",[workflow],|r|r.get(0))?;
    let completed_events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_completed'",[workflow],|r|r.get(0))?;
    ensure!(pending+completed_count==count && completed==completed_count && completed_events==completed_count,"owner publication pending/completion history incomplete");Ok(())
}

/// Current metadata projection only. It cannot attest physical inventory,
/// resource quiescence, publication completion, or provider/input permission.
pub(crate) struct OwnerPublicationNativeFacts {facts:super::native_handoff::NativeHandoffFacts}
#[cfg(test)]
pub(super) fn current_facts_fixture(facts:super::native_handoff::NativeHandoffFacts)->OwnerPublicationNativeFacts {OwnerPublicationNativeFacts {facts}}
impl OwnerPublicationNativeFacts {
    pub(super) fn snapshot_facts(&self)->&super::native_handoff::NativeHandoffFacts {&self.facts}
    pub(super) fn from_recovery_metadata(facts:super::native_handoff::NativeHandoffFacts)->Self {Self {facts}}
    pub(crate) fn namespace(&self)->&augmentagent_channel_core::native_vm::ownership::Authority {&self.facts.source_owners[0]}
    pub(crate) fn original_binding(&self)->&augmentagent_channel_core::native_vm::ownership::TransferBinding {&self.facts.original_binding}
    pub(crate) fn fingerprint(&self)->Result<String> {
        Ok(format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::json!({
            "version":1,"workflow":self.facts.workflow_id,"binding":self.facts.workflow_binding,"namespace":self.namespace(),
            "original_binding":self.original_binding(),"source":self.facts.source_registration_sha256,"latest":self.facts.latest_input_sha256}))?)))
    }
    /// Descriptive latest SQL launches, never prepared/native dispatch proof.
    pub(crate) fn completion_launches(&self)->Vec<super::native_launch::NativeLaunchRecord> {
        self.facts.runtimes.iter().map(|runtime|runtime.latest_launch.clone()).collect()
    }
    pub(crate) fn runtimes(&self)->Result<Vec<augmentagent_channel_core::native_vm::ownership::PreparationRuntime>> {
        use augmentagent_channel_core::native_vm::ownership::{PreparationRuntime,RuntimeInventoryRow};
        self.facts.runtimes.iter().map(|runtime| {
            let inventory=if let Some(row)=runtime.latest_observation.as_ref().filter(|row|row.certainty==super::native_observation::Certainty::Certain
                && row.launch_intent_id==runtime.latest_launch.intent.intent_id) {
                let mut inventory:RuntimeInventoryRow=serde_json::from_value(row.proof.as_ref().context("native inventory proof missing")?["inventory"].clone())?;
                inventory.effective_owner=runtime.effective_authority.owner.clone();inventory.effective_audit_path=runtime.effective_authority.audit_path.clone();Some(inventory)
            }else {None};
            Ok(PreparationRuntime {runtime_relative_path:runtime.latest_launch.intent.runtime_relative_path.clone(),
                creation_receipt_sha256:runtime.latest_launch.intent.creation_receipt_sha256.clone(),authority:runtime.effective_authority.clone(),
                launch_sha256:runtime.latest_launch.intent.launch_sha256.clone(),inventory})
        }).collect()
    }
}
/// Minted only for the consuming original private activation. No Clone,
/// serialization, raw-turn accessor or conversion into execution authority.
/// Arc sharing retains this same metadata role for its logger and live guard.
pub(crate) struct LiveOwnerTurn {
    turn:owner_turn::OwnerTurn,preparation:OwnerPreparationBinding,native_audit:std::path::PathBuf,intent:Reference,
}
fn require_live_metadata(tx:&Transaction<'_>,metadata:&LiveOwnerTurn,native_audit:&Path,now:i64)->Result<()> {
    ensure!(metadata.native_audit==native_audit,"live owner metadata native namespace changed");
    require_metadata(tx,&metadata.turn,&metadata.preparation,native_audit,now)?;
    let raw:String=tx.query_row("SELECT record FROM workflow_owner_publication_intent WHERE request_id=?1",[&metadata.turn.request_id],|r|r.get(0))?;
    require(tx,&metadata.turn.request_id,&metadata.intent,&raw)?;
    Ok(())
}
fn require_metadata(tx:&Transaction<'_>,turn:&owner_turn::OwnerTurn,preparation:&OwnerPreparationBinding,native_audit:&Path,now:i64)->Result<()> {
        require_native_dispatch_tx(tx,&turn.lease,&turn.binding,now)?;
        owner_turn::validate_turn(tx,turn,now)?;
        owner_revision::require_preparation(tx,turn,preparation)?;
        let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_publication_pending WHERE workflow=?1 AND request_id=?2)",params![turn.lease.workflow,turn.request_id],|r|r.get(0))?;
        let completed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_publication_completion WHERE workflow=?1 AND request_id=?2)",params![turn.lease.workflow,turn.request_id],|r|r.get(0))?;
        ensure!(pending!=completed,"original owner publication metadata disposition changed");
        let raw:String=tx.query_row("SELECT record FROM workflow_owner_publication_intent WHERE request_id=?1",[&turn.request_id],|r|r.get(0))?;
        let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
        ensure!(record.workflow()==turn.lease.workflow && record.generation()==turn.lease.generation,"owner publication activated native generation changed");
        record.validate_current_native(tx,native_audit)?;
    Ok(())
}
impl WorkflowStore {
    /// Called after the original guard was consumed by private activation and
    /// its original-store handoff settled. A saved turn cannot supply the
    /// opaque original publication draft required by this producer.
    pub(crate) fn bind_live_owner_publication(&mut self,turn:owner_turn::OwnerTurn,publication:&crate::self_improve::OwnerPublicationDraft<'_>,
        native_audit:&Path,now:i64)->Result<LiveOwnerTurn> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_metadata(&tx,&turn,publication.preparation(),native_audit,now)?;publication.require_metadata_record(&tx,&turn)?;
        let raw:String=tx.query_row("SELECT record FROM workflow_owner_publication_intent WHERE request_id=?1",[&turn.request_id],|r|r.get(0))?;
        let metadata=LiveOwnerTurn {turn,preparation:publication.preparation().clone(),native_audit:native_audit.into(),intent:reference(&raw)};
        require_live_metadata(&tx,&metadata,native_audit,now)?;tx.commit()?;Ok(metadata)
    }
    /// Inspection under the actual retained original metadata turn. This mints
    /// no ordinary writer token and cannot promote that turn to execution.
    pub(crate) fn read_owner_publication_metadata<T>(&mut self,metadata:&LiveOwnerTurn,publication:&crate::self_improve::OwnerPublicationDraft<'_>,
        native_audit:&Path,now:i64,inspect:impl FnOnce(&OwnerPublicationReadContext<'_>)->Result<T>)->Result<T> {
        let turn=&metadata.turn;let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_live_metadata(&tx,metadata,native_audit,now)?;publication.require_metadata_record(&tx,turn)?;
        let reader=OwnerPublicationReadContext {conn:&tx,workflow:&turn.lease.workflow,records:std::cell::RefCell::new(None)};
        let result=inspect(&reader)?;
        require_live_metadata(&tx,metadata,native_audit,now)?;tx.commit()?;Ok(result)
    }
    /// Actual native -> scope -> immediate workflow transaction -> bounded FS.
    /// This records metadata only and cannot promote the original logger/guard.
    pub(crate) fn complete_owner_publication(&mut self,metadata:&LiveOwnerTurn,publication:&crate::self_improve::OwnerPublicationDraft<'_>,
        proof:&augmentagent_channel_core::native_vm::ownership::MetadataNativeOwnerProof<'_>,transition:&augmentagent_channel_core::autoship_write_scope::ScopeTransition,now:i64)->Result<()> {
        let turn=&metadata.turn;
        ensure!(proof.namespace().owner.workflow_id==turn.lease.workflow && proof.namespace().owner.lease_generation==turn.lease.generation,"owner publication proof authority differs from current turn");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts=Self::publication_native_facts_tx(&tx,metadata,publication,Path::new(&proof.namespace().audit_path),now)?;
        ensure!(facts.namespace()==proof.namespace() && facts.original_binding()==proof.binding(),"owner publication original native proof changed");
        let raw:String=tx.query_row("SELECT record FROM workflow_owner_publication_intent WHERE request_id=?1",[&turn.request_id],|r|r.get(0))?;
        let intent:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
        let existing=completion(&tx,&turn.lease.workflow,&turn.request_id,&intent,&raw)?;
        let current=||require_live_metadata(&tx,metadata,Path::new(&proof.namespace().audit_path),chrono::Utc::now().timestamp_millis());
        let access=OwnerPublicationCommitMetadata {_transaction:&tx,workflow:&turn.lease.workflow,request:&turn.request_id,intent_sha256:reference(&raw).record_sha256,authority:crate::self_improve::OwnerPublicationAuthority::live(),current:&current};
        let completed=publication.commit_canonical(&access,proof,transition,&facts.fingerprint()?,existing.as_ref(),now)?;
        completed.validate(&tx,&intent,&raw)?;
        if existing.is_none() {
            let raw=serde_json::to_string(&completed)?;ensure!(raw.len()<=32768,"owner publication completion exceeds bound");let sha=reference(&raw).record_sha256;
            event(&tx,&turn.lease.workflow,turn.lease.generation,"owner_publication_completed",&serde_json::json!({"version":1,"request_id":turn.request_id,"record_sha256":sha,"intent_sha256":completed.intent_sha256()}).to_string(),now)?;
            let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&turn.lease.workflow],|r|r.get(0))?;
            tx.execute("INSERT INTO workflow_owner_publication_completion VALUES(?1,?2,?3,?4,?5)",params![turn.request_id,turn.lease.workflow,sequence,sha,raw])?;
            ensure!(tx.execute("DELETE FROM workflow_owner_publication_pending WHERE workflow=?1 AND request_id=?2",params![turn.lease.workflow,turn.request_id])?==1,"owner publication pending pointer changed at completion");
        }
        require_live_metadata(&tx,metadata,Path::new(&proof.namespace().audit_path),now)?;
        publication.require_metadata_record(&tx,turn)?;validate_history(&tx,&turn.lease.workflow)?;tx.commit()?;Ok(())
    }
    /// Current metadata authority for an uninterrupted original activation.
    /// This deliberately does not assert publication completion or coding readiness.
    pub(crate) fn validate_owner_publication_metadata(&mut self,metadata:&LiveOwnerTurn,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_live_metadata(&tx,metadata,&metadata.native_audit,now)?;tx.commit()?;Ok(())
    }
    fn publication_native_facts_tx(tx:&Transaction<'_>,metadata:&LiveOwnerTurn,publication:&crate::self_improve::OwnerPublicationDraft<'_>,native_audit:&Path,now:i64)->Result<OwnerPublicationNativeFacts> {
        let turn=&metadata.turn;require_live_metadata(tx,metadata,native_audit,now)?;
        publication.require_metadata_record(tx,turn)?;
        let namespace=augmentagent_channel_core::native_vm::ownership::Authority {owner:augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:turn.lease.workflow.clone(),lease_generation:turn.lease.generation},audit_path:native_audit.to_str().context("native metadata audit is not UTF-8")?.into()};
        let facts=super::native_handoff::read_current_metadata_facts(tx,&turn.lease,&turn.binding,&namespace,now)?;
        publication.validate_native_binding(&facts.original_binding)?;
        Ok(OwnerPublicationNativeFacts {facts})
    }
    pub(crate) fn owner_publication_native_facts(&mut self,metadata:&LiveOwnerTurn,publication:&crate::self_improve::OwnerPublicationDraft<'_>,native_audit:&Path,now:i64)->Result<OwnerPublicationNativeFacts> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts=Self::publication_native_facts_tx(&tx,metadata,publication,native_audit,now)?;tx.commit()?;Ok(facts)
    }
    pub(crate) fn revalidate_owner_publication_native_facts(&mut self,metadata:&LiveOwnerTurn,publication:&crate::self_improve::OwnerPublicationDraft<'_>,native_audit:&Path,expected:&OwnerPublicationNativeFacts,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual=Self::publication_native_facts_tx(&tx,metadata,publication,native_audit,now)?;
        ensure!(actual.fingerprint()?==expected.fingerprint()?,"owner publication native metadata changed");tx.commit()?;Ok(())
    }
}

impl WorkflowStore {
    /// The caller holds the recovered preparation's original native proof and
    /// scope transition. No new native lock or provider call enters this SQL TX.
    /// Only changes made by this exclusively borrowed connection count. A
    /// validated existing completion must not restart progress/backoff clocks.
    pub(crate) fn complete_recovered_owner_publication_progress(&mut self,
        lease:&super::owner_publication_recovery::RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        prepared:&crate::self_improve::RecoveredOwnerPreparation<'_,'_,'_>,
        transition:&augmentagent_channel_core::autoship_write_scope::ScopeTransition,now:i64,
    )->Result<bool> {
        let before:i64=self.conn.query_row("SELECT total_changes()",[],|r|r.get(0))?;
        self.complete_recovered_owner_publication(lease,authorization,prepared,transition,now)?;
        let after:i64=self.conn.query_row("SELECT total_changes()",[],|r|r.get(0))?;
        ensure!(after>=before,"publication recovery write counter decreased");
        Ok(after>before)
    }
    pub(crate) fn complete_recovered_owner_publication(&mut self,lease:&super::owner_publication_recovery::RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        prepared:&crate::self_improve::RecoveredOwnerPreparation<'_,'_,'_>,
        transition:&augmentagent_channel_core::autoship_write_scope::ScopeTransition,now:i64,
    )->Result<()> {
        prepared.require_recovery_claim(lease)?;
        // This captured fence may query policy and the controller's independent
        // connection. It must run before this connection holds a transaction.
        prepared.validate_native_current()?;
        let expected=self.owner_publication_recovery_native_facts(lease,authorization,now)?;
        ensure!(expected.namespace()==prepared.native_namespace() && expected.original_binding()==prepared.native_binding(),"recovered publication native proof differs from original claim");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts=super::owner_publication_recovery::recovery_native_facts_tx(&tx,lease,authorization,now)?;
        ensure!(facts.fingerprint()?==expected.fingerprint()?,"recovered publication native facts changed before transaction");
        let (intent,raw)=prepared.publication_record(&tx)?;
        ensure!(intent.request()==lease.request_id(),"recovered publication request changed");
        let existing=completion(&tx,intent.workflow(),intent.request(),&intent,&raw)?;
        let current=||super::owner_publication_recovery::require_authorized_claim(&tx,lease,authorization,chrono::Utc::now().timestamp_millis());
        let access=OwnerPublicationCommitMetadata {_transaction:&tx,workflow:intent.workflow(),request:intent.request(),intent_sha256:reference(&raw).record_sha256,
            authority:crate::self_improve::OwnerPublicationAuthority::recovery(lease.publication_reference()),current:&current};
        let completed=prepared.commit_canonical(&access,transition,&facts.fingerprint()?,existing.as_ref(),now)?;
        completed.validate(&tx,&intent,&raw)?;
        if existing.is_none() {
            let completed_raw=serde_json::to_string(&completed)?;ensure!(completed_raw.len()<=32768,"recovered publication completion exceeds bound");let sha=reference(&completed_raw).record_sha256;
            event(&tx,intent.workflow(),completed.generation(),"owner_publication_completed",&serde_json::json!({"version":1,"request_id":intent.request(),"record_sha256":sha,"intent_sha256":completed.intent_sha256()}).to_string(),now)?;
            let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[intent.workflow()],|r|r.get(0))?;
            tx.execute("INSERT INTO workflow_owner_publication_completion VALUES(?1,?2,?3,?4,?5)",params![intent.request(),intent.workflow(),sequence,sha,completed_raw])?;
            ensure!(tx.execute("DELETE FROM workflow_owner_publication_pending WHERE workflow=?1 AND request_id=?2",params![intent.workflow(),intent.request()])?==1,"recovered publication pending pointer changed");
        }
        current()?;
        prepared.publication_record(&tx)?;validate_history(&tx,intent.workflow())?;
        tx.commit()?;prepared.validate_native_current()?;Ok(())
    }
}
