//! Bounded private canonical commit under original native -> scope -> SQL locks.
//! Completion is immutable descriptive history, never productive admission.
use super::*;
use augmentagent_channel_core::{autoship_write_scope::{self,DeclaredPath,ScopeReference,ScopeTransition},native_vm::ownership::{Authority,MetadataNativeOwnerProof}};

fn sha(value:&str)->bool {value.len()==64 && value.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'))}
/// Strict saved authority identity, not permission to perform a new write.
#[derive(Clone,Serialize,Deserialize)]
#[serde(untagged)]
pub(crate) enum OwnerPublicationAuthority {Live(LivePublicationAuthority),Recovery(RecoveryPublicationAuthority)}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) enum LivePublicationAuthority {#[serde(rename="live_owner_turn")]Original}
#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryPublicationAuthority {kind:RecoveryPublicationKind,claim:crate::autoship_workflow::owner_publication_recovery::PublicationRecoveryReference}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) enum RecoveryPublicationKind {#[serde(rename="recovery_lease")]Metadata}
impl OwnerPublicationAuthority {
    pub(crate) fn live()->Self {Self::Live(LivePublicationAuthority::Original)}
    pub(crate) fn recovery(claim:crate::autoship_workflow::owner_publication_recovery::PublicationRecoveryReference)->Self {
        Self::Recovery(RecoveryPublicationAuthority {kind:RecoveryPublicationKind::Metadata,claim})
    }
    fn generation(&self,activated:i64)->i64 {match self {Self::Live(_)=>activated,Self::Recovery(value)=>value.claim.generation()}}
    fn sequence(&self)->Option<i64> {match self {Self::Live(_)=>None,Self::Recovery(value)=>Some(value.claim.sequence())}}
    fn validate_event_position(&self,conn:&rusqlite::Connection,intent:&OwnerPublicationRecord,sequence:i64)->Result<()> {
        use crate::autoship_workflow::owner_publication_recovery as recovery;
        match self {Self::Live(_)=>recovery::validate_live_publication_event_position(conn,intent.workflow(),intent.request(),sequence),
            Self::Recovery(value)=>recovery::validate_publication_event_position(conn,intent.workflow(),intent.request(),&value.claim,sequence)}
    }
    fn validate(&self,conn:&rusqlite::Connection,intent:&OwnerPublicationRecord,raw:&str,at:i64)->Result<()> {
        match self {Self::Live(_)=>Ok(()),Self::Recovery(value)=>{
            ensure!(value.claim.generation()>intent.generation(),"recovery publication generation did not advance");
            crate::autoship_workflow::owner_publication_recovery::validate_publication_reference(conn,intent.workflow(),intent.request(),raw,&value.claim,at)
        }}
    }
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerPublicationCompletion {
    version:u32,workflow:String,request_id:String,intent_sha256:String,
    original_generation:i64,activated_generation:i64,authority:OwnerPublicationAuthority,
    native_namespace:Authority,native_binding_sha256:String,native_facts_sha256:String,native_evidence:Evidence,
    scope:ScopeReference,canonical_sha256:String,evidence:Evidence,at_ms:i64,
}
impl OwnerPublicationCompletion {
    pub(crate) fn scope_reference(&self)->&ScopeReference {&self.scope}
    pub(crate) fn workflow(&self)->&str {&self.workflow}
    pub(crate) fn request(&self)->&str {&self.request_id}
    pub(crate) fn generation(&self)->i64 {self.authority.generation(self.activated_generation)}
    pub(crate) fn recovery_sequence(&self)->Option<i64> {self.authority.sequence()}
    pub(crate) fn at_ms(&self)->i64 {self.at_ms}
    pub(crate) fn intent_sha256(&self)->&str {&self.intent_sha256}
    pub(crate) fn native_bytes(&self)->usize {self.native_evidence.bytes}
    pub(crate) fn validate_event_position(&self,conn:&rusqlite::Connection,intent:&OwnerPublicationRecord,sequence:i64)->Result<()> {
        self.authority.validate_event_position(conn,intent,sequence)
    }
    pub(crate) fn validate(&self,conn:&rusqlite::Connection,intent:&OwnerPublicationRecord,raw:&str)->Result<()> {
        ensure!(serde_json::to_vec(self)?.len()<=32*1024 && self.version==1
            && self.workflow==intent.workflow() && self.request_id==intent.request() && self.intent_sha256==amendment::digest(raw.as_bytes())
            && self.original_generation==intent.input.generation && self.activated_generation==intent.generation()
            && self.native_namespace.owner.workflow_id==self.workflow && self.native_namespace.owner.lease_generation==self.activated_generation
            && self.native_binding_sha256==intent.original_native.binding_sha256 && sha(&self.native_facts_sha256)
            && self.scope.version==1 && self.scope.worktree==intent.candidate.root_path() && self.scope.contract_hash==intent.next_binding.contract && sha(&self.scope.record_sha256)
            && self.canonical_sha256==amendment::digest(intent.next_manifest.as_bytes()) && self.evidence==intent.next_evidence && self.at_ms>=0,
            "owner publication completion original lineage changed");
        self.authority.validate(conn,intent,raw,self.at_ms)?;
        let (audit,audit_len):(String,i64)=conn.query_row("SELECT substr(audit_path,1,4097),length(CAST(audit_path AS BLOB)) FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",rusqlite::params![self.workflow,self.activated_generation],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!((1..=4096).contains(&audit_len) && audit.len() as i64==audit_len && self.native_namespace.audit_path==audit && self.native_evidence.bytes<=1024*1024 && sha(&self.native_evidence.sha256)
            && self.native_evidence.file==format!("{}-owner-{}-native-{}.json",intent.input.source.key(),amendment::digest(self.request_id.as_bytes()),self.native_evidence.sha256),
            "owner publication completion native reference changed");Ok(())
    }
}
struct ControlDirectory {file:std::fs::File,path:PathBuf,device:u64,inode:u64}
impl ControlDirectory {
    fn open(path:&Path)->Result<Self> {
        let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_DIRECTORY).open(path)?;
        let metadata=file.metadata()?;
        let result=Self {file,path:path.into(),device:metadata.dev(),inode:metadata.ino()};result.validate()?;Ok(result)
    }
    fn validate(&self)->Result<()> {
        let metadata=std::fs::symlink_metadata(&self.path)?;
        ensure!(metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid()==unsafe{libc::geteuid()}
            && metadata.mode()&0o022==0 && metadata.dev()==self.device && metadata.ino()==self.inode,
            "owner publication protected directory changed");Ok(())
    }
    fn immutable(&self,name:&str,data:&[u8],max:usize)->Result<()> {
        self.validate()?;ensure!(data.len()<=max,"owner publication immutable artifact exceeds bound");
        let path=self.path.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_)=>ensure!(bytes(&path,max)?==data,"owner publication immutable artifact changed"),
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{
                let mut staged=tempfile::NamedTempFile::new_in(&self.path)?;staged.write_all(data)?;staged.as_file().sync_all()?;
                self.validate()?;staged.persist_noclobber(&path).map_err(|e|e.error)?;
            },Err(e)=>return Err(e.into()),
        }
        self.file.sync_all()?;self.validate()
    }
}
fn permissions(prepared:&PreparedAcceptance)->Result<(Vec<DeclaredPath>,Vec<String>)> {
    prepared.verify_registration()?;
    let allowed=prepared.proposal.contract.allowed_paths.iter().map(|scope|match scope {
        crate::autoship_contract::PathScope::File(path)=>DeclaredPath {path:path.clone(),directory:false},
        crate::autoship_contract::PathScope::Directory(path)=>DeclaredPath {path:path.clone(),directory:true},
    }).collect();Ok((allowed,prepared.proposal.files.keys().cloned().collect()))
}
impl OwnerPublicationDraft<'_> {
    fn next_scope(&self)->Result<ScopeReference> {
        let (allowed,pinned)=permissions(self.producer.prepared())?;
        autoship_write_scope::scope_reference_for(self.record.candidate.root_path(),&self.record.next_binding.contract,allowed,pinned)
    }
    fn previous_scope(&self)->Result<Option<ScopeReference>> {
        let Some(prepared)=self.record.previous_prepared()? else {return Ok(None)};
        let registration=&prepared.baseline.registration;
        let (allowed,pinned)=permissions(&prepared)?;
        Ok(Some(autoship_write_scope::scope_reference_for(self.record.candidate.root_path(),registration.contract_hash(),allowed,pinned)?))
    }
    pub(in crate::self_improve) fn with_scope_transition<T>(&self,visit:impl FnOnce(&ScopeTransition)->Result<T>)->Result<T> {
        let is_next=self.record.canonical.publication_is_next(&self.record.next_manifest)?;
        let old=self.previous_scope()?;
        let next=self.next_scope()?;
        let (allowed,pinned)=permissions(self.producer.prepared())?;
        autoship_write_scope::with_scope_transition(self.record.candidate.root_path(),if is_next {Some(&next)}else{old.as_ref()},
            &self.record.next_binding.contract,allowed,pinned,visit)
    }
    /// Called only inside the current workflow transaction while the actual
    /// original native proof and scope transition remain borrowed and locked.
    pub(crate) fn commit_canonical(&self,access:&crate::autoship_workflow::OwnerPublicationCommitMetadata<'_>,proof:&MetadataNativeOwnerProof<'_>,transition:&ScopeTransition,
        facts_sha256:&str,existing:Option<&OwnerPublicationCompletion>,now:i64)->Result<OwnerPublicationCompletion> {
        access.require(self.record.workflow(),self.record.request(),&amendment::digest(serde_json::to_string(&self.record)?.as_bytes()))?;
        self.validate_native_binding(proof.binding())?;
        ensure!(proof.namespace().owner.workflow_id==self.record.workflow() && proof.namespace().owner.lease_generation==self.record.generation(),"owner publication native proof namespace changed");
        let control=ControlDirectory::open(&self.record.canonical.control)?;
        let is_next=self.record.canonical.publication_is_next(&self.record.next_manifest)?;
        self.previous_scope()?;
        self.verify_commit_files()?;
        if let Some(existing)=existing {
            ensure!(is_next && existing.scope==self.next_scope()? && existing.native_namespace==*proof.namespace()
                && existing.native_facts_sha256==facts_sha256,"owner publication completion no longer matches current original proof");
            let saved=bytes(&control.path.join(&existing.native_evidence.file),1024*1024)?;
            ensure!(saved.len()==existing.native_evidence.bytes && amendment::digest(&saved)==existing.native_evidence.sha256,"owner publication original native evidence changed");
            let saved:serde_json::Value=serde_json::from_slice(&saved)?;
            ensure!(saved["namespace"]==serde_json::to_value(proof.namespace())? && saved["original_binding_sha256"]==existing.native_binding_sha256,
                "owner publication original native evidence lineage changed");
        }
        // Bound both descriptive records before any scope/file mutation.
        let fresh=if existing.is_none() {Some(self.fresh_completion(access.authority(),proof,facts_sha256,now)?)}else{None};
        // All workflow/current-authority and completed-history checks precede
        // mutation. A reverted completed canonical cannot republish its scope.
        access.require(self.record.workflow(),self.record.request(),&amendment::digest(serde_json::to_string(&self.record)?.as_bytes()))?;
        let scope=transition.register()?;
        ensure!(scope.reference()==&self.next_scope()?,"owner publication registered scope differs from original preparation");
        if let Some(existing)=existing {
            control.file.sync_all()?;control.validate()?;scope.validate_current()?;
            return Ok(serde_json::from_slice(&serde_json::to_vec(existing)?)?);
        }
        let (completed,evidence)=fresh.context("owner publication fresh completion missing")?;
        control.immutable(&completed.native_evidence.file,&evidence,1024*1024)?;
        access.require(self.record.workflow(),self.record.request(),&amendment::digest(serde_json::to_string(&self.record)?.as_bytes()))?;
        if !is_next {
            let mut staged=tempfile::NamedTempFile::new_in(&control.path)?;staged.write_all(self.record.next_manifest.as_bytes())?;staged.as_file().sync_all()?;
            control.validate()?;
            ensure!(!self.record.canonical.publication_is_next(&self.record.next_manifest)?,"owner canonical changed during publication");
            let path=control.path.join(format!("{}-workflow.json",self.record.input.source.key()));
            if self.record.canonical.manifest.is_some() {staged.persist(&path).map_err(|e|e.error)?;}
            else {staged.persist_noclobber(&path).map_err(|e|e.error)?;}
        }
        control.file.sync_all()?;control.validate()?;
        ensure!(self.record.canonical.publication_is_next(&self.record.next_manifest)?,"owner publication next canonical missing");
        self.verify_commit_files()?;scope.validate_current()?;
        access.require(self.record.workflow(),self.record.request(),&amendment::digest(serde_json::to_string(&self.record)?.as_bytes()))?;
        Ok(completed)
    }
    fn fresh_completion(&self,authority:&OwnerPublicationAuthority,proof:&MetadataNativeOwnerProof<'_>,facts_sha256:&str,now:i64)->Result<(OwnerPublicationCompletion,Vec<u8>)> {
        let evidence=augmentagent_channel_core::native_vm::ownership::canonical_bytes(proof.evidence())?;
        ensure!(evidence.len()<=1024*1024 && sha(facts_sha256) && now>=0,"owner publication native evidence or metadata exceeds bound");
        let evidence_sha=amendment::digest(&evidence);
        let native_evidence=Evidence {file:format!("{}-owner-{}-native-{}.json",self.record.input.source.key(),amendment::digest(self.record.request().as_bytes()),evidence_sha),bytes:evidence.len(),sha256:evidence_sha};
        let completed=OwnerPublicationCompletion {version:1,workflow:self.record.workflow().into(),request_id:self.record.request().into(),
            intent_sha256:amendment::digest(serde_json::to_string(&self.record)?.as_bytes()),original_generation:self.record.input.generation,
            activated_generation:self.record.generation(),authority:authority.clone(),native_namespace:proof.namespace().clone(),
            native_binding_sha256:self.record.original_native.binding_sha256.clone(),native_facts_sha256:facts_sha256.into(),native_evidence,
            scope:self.next_scope()?,canonical_sha256:amendment::digest(self.record.next_manifest.as_bytes()),evidence:self.record.next_evidence.clone(),at_ms:now};
        ensure!(serde_json::to_vec(&completed)?.len()<=32768,"owner publication completion exceeds bound before mutation");Ok((completed,evidence))
    }
    fn verify_commit_files(&self)->Result<()> {
        self.producer.candidate.verify_current()?;
        ensure!(self.producer.activation_binding()?==self.record.preparation,"owner publication original artifact changed at commit");
        let evidence=bytes(&self.record.canonical.control.join(&self.record.next_evidence.file),4*1024*1024)?;
        ensure!(evidence.len()==self.record.next_evidence.bytes && amendment::digest(&evidence)==self.record.next_evidence.sha256
            && evidence==serde_json::to_vec(self.producer.prepared())?,"owner publication immutable evidence changed at commit");Ok(())
    }
}

fn counted_bytes(path:&Path,max:usize,used:&mut usize)->Result<Vec<u8>> {
    let raw=bytes(path,max)?;*used=used.checked_add(raw.len()).context("canonical lineage size overflow")?;
    ensure!(*used<=128*1024*1024,"canonical lineage exceeds byte bound");Ok(raw)
}
impl OwnerPublicationRecord {
    pub(crate) fn validate_completed_canonical_files(&self,completed:&OwnerPublicationCompletion)->Result<()> {
        ensure!(self.canonical.publication_is_next(&self.next_manifest)?,"recovery original completed canonical changed");
        self.read_canonical_ancestor(&self.canonical.control,self.input.source,&self.next_manifest,completed,&mut 0)?;Ok(())
    }
    /// Historical criteria inspection only. Protected saved evidence is data,
    /// never fresh native/resource/source proof or executable preparation.
    pub(crate) fn read_canonical_ancestor(&self,control:&Path,source:AcceptanceSource,manifest:&str,
        completed:&OwnerPublicationCompletion,used:&mut usize)->Result<Option<String>> {
        ensure!(control.canonicalize()?==self.canonical.control && source==self.input.source && manifest==self.next_manifest,
            "owner canonical lineage differs from original protected namespace");
        let directory=ControlDirectory::open(&self.canonical.control)?;
        let raw=counted_bytes(&self.preparation_file,4*1024*1024,used)?;
        let stored:Stored=serde_json::from_slice(&raw)?;
        ensure!(stored.version==3 && stored.input==self.input && stored.candidate==self.candidate && stored.canonical==self.canonical
            && serde_json::to_vec(&stored)?==raw,"owner canonical lineage original preparation changed");
        stored.prepared.verify_registration()?;
        let binding=crate::autoship_workflow::OwnerPreparationBinding::from_artifact(amendment::digest(&raw),self.input.key()?,
            &stored.prepared.baseline.registration,&stored.prepared.baseline.receipts)?;
        ensure!(binding==self.preparation,"owner canonical lineage preparation binding changed");
        let evidence=counted_bytes(&directory.path.join(&self.next_evidence.file),4*1024*1024,used)?;
        ensure!(evidence.len()==self.next_evidence.bytes && amendment::digest(&evidence)==self.next_evidence.sha256
            && evidence==serde_json::to_vec(&stored.prepared)?,"owner canonical lineage evidence changed");
        let previous=if let Some(raw)=&self.canonical.manifest {
            let old:WorkflowManifest=serde_json::from_str(raw)?;
            let path=if let Some(name)=&old.evidence_file {
                ensure!(Path::new(name).components().count()==1 && matches!(Path::new(name).components().next(),Some(std::path::Component::Normal(_))),"owner canonical ancestor evidence is not a basename");directory.path.join(name)
            }else {evidence_path(&directory.path,source,&old.original_base)?};
            let old_prepared:PreparedAcceptance=serde_json::from_slice(&counted_bytes(&path,4*1024*1024,used)?)?;
            old_prepared.verify_registration()?;
            let registration=&old_prepared.baseline.registration;
            ensure!(registration.contract_hash()==old.contract_hash && registration.contract().revision==old.contract_revision
                && registration.baseline_head()==old.original_base && registration.contract().source()?==source,"owner canonical ancestor registration changed");
            Some(old_prepared)
        }else{None};
        self.input.validate_prepared(&stored.prepared,previous.as_ref().map(|p|&p.baseline.registration))?;
        let native=counted_bytes(&directory.path.join(&completed.native_evidence.file),1024*1024,used)?;
        ensure!(native.len()==completed.native_evidence.bytes && amendment::digest(&native)==completed.native_evidence.sha256,"owner canonical lineage native evidence changed");
        let native:serde_json::Value=serde_json::from_slice(&native)?;
        ensure!(native["version"]==1 && native["kind"]=="retained_native_metadata_quiescence" && native["namespace"]==serde_json::to_value(&completed.native_namespace)?
            && native["original_binding_sha256"]==completed.native_binding_sha256,"owner canonical lineage native reference changed");
        directory.validate()?;Ok(self.canonical.manifest.clone())
    }
}
