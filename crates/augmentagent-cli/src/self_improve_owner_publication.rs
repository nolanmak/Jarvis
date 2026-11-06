//! Original publication intent and proof-bound private metadata commit; never execution.
use super::*;
use anyhow::{bail,ensure,Context,Result};
use std::io::{Read,Write};
use std::os::unix::fs::{MetadataExt,OpenOptionsExt,PermissionsExt};
const MAX_INTENT:usize=4*1024*1024+32*1024;
#[path="self_improve_owner_publication_commit.rs"]
mod commit;
pub(crate) use commit::OwnerPublicationCompletion;
fn bytes(path:&Path,max:usize)->Result<Vec<u8>> {
    let mut file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    let m=file.metadata()?;ensure!(m.is_file() && m.nlink()==1 && m.uid()==unsafe{libc::geteuid()} && m.mode()&0o077==0 && m.len()<=max as u64,"owner publication file is not private/bounded");
    let mut bytes=Vec::new();(&mut file).take(max as u64+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=max && bytes.len() as u64==m.len(),"owner publication file changed while reading");Ok(bytes)
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CanonicalSnapshot {version:u32,control:PathBuf,source:AcceptanceSource,manifest:Option<String>}
impl CanonicalSnapshot {
    pub(super) fn capture(workflow:&workflow::WorkflowGuard,control:&Path,source:AcceptanceSource,previous:Option<&RegisteredContract>)->Result<Self> {
        let manifest=guarded_original_manifest(workflow,control,source)?;
        match (manifest.as_ref(),previous) {
            (None,None)=>(),
            (Some(saved),Some(previous))=>ensure!(saved.contract_hash==previous.contract_hash() && saved.original_base==previous.baseline_head() && saved.contract_revision==previous.contract().revision,"owner preparation canonical registration differs from original"),
            _=>bail!("owner preparation canonical acceptance is present or missing unexpectedly"),
        }
        let path=control.join(format!("{}-workflow.json",source.key()));
        let raw=manifest.as_ref().map(|_|bytes(&path,4096).and_then(|b|Ok(String::from_utf8(b)?))).transpose()?;
        let result=Self {version:1,control:control.canonicalize()?,source,manifest:raw};result.verify_current()?;Ok(result)
    }
    #[cfg(test)]
    pub(super) fn absent(control:&Path,source:AcceptanceSource)->Self {Self {version:1,control:control.canonicalize().unwrap(),source,manifest:None}}
    fn require_no_orphan(&self)->Result<()> {
        if self.manifest.is_none() {
            let prefix=format!("{}-base-",self.source.key());let mut count=0;
            for entry in std::fs::read_dir(&self.control)? {let entry=entry?;count+=1;ensure!(count<=4096,"owner canonical namespace exceeds bound");
                let name=entry.file_name();let name=name.to_string_lossy();ensure!(!(name.starts_with(&prefix) && name.ends_with(".json")),"owner canonical absence has an orphan original baseline");}
        }
        Ok(())
    }
    /// Exact old or exact next only. Preparation/activation still use the
    /// stricter old-only check. Initial next replay must retain orphan checks.
    fn publication_is_next(&self,next:&str)->Result<bool> {
        ensure!(self.version==1 && self.control.is_absolute(),"owner canonical snapshot version/path changed");
        amendment::require_no_pending(&self.control,self.source)?;self.require_no_orphan()?;
        let path=self.control.join(format!("{}-workflow.json",self.source.key()));
        match std::fs::symlink_metadata(&path) {
            Ok(_)=>{let raw=bytes(&path,4096)?;if raw==next.as_bytes() {return Ok(true);}
                ensure!(self.manifest.as_ref().map(|m|m.as_bytes())==Some(raw.as_slice()),"owner publication canonical state is neither original nor next");Ok(false)},
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{ensure!(self.manifest.is_none(),"owner publication original canonical disappeared");Ok(false)},
            Err(e)=>Err(e.into()),
        }
    }
    pub(super) fn verify_current(&self)->Result<()> {
        ensure!(self.version==1 && self.control.is_absolute(),"owner canonical snapshot version/path changed");
        amendment::require_no_pending(&self.control,self.source)?;
        let path=self.control.join(format!("{}-workflow.json",self.source.key()));
        match (&self.manifest,std::fs::symlink_metadata(&path)) {
            (Some(expected),Ok(_))=>ensure!(bytes(&path,4096)?==expected.as_bytes(),"owner preparation canonical manifest changed"),
            (None,Err(e)) if e.kind()==std::io::ErrorKind::NotFound=>{
                self.require_no_orphan()?;
            },_=>bail!("owner preparation canonical manifest appeared or disappeared"),
        }
        Ok(())
    }
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {file:String,bytes:usize,sha256:String}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Native {audit:String,binding_sha256:String}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerPublicationRecord {
    version:u32,repository:String,input:Input,candidate:CandidateIdentity,canonical:CanonicalSnapshot,
    preparation:crate::autoship_workflow::OwnerPreparationBinding,preparation_file:PathBuf,
    original_native:Native,next_binding:crate::autoship_workflow::Binding,activated_generation:i64,
    next_manifest:String,next_evidence:Evidence,
}
impl OwnerPublicationRecord {
    pub(crate) fn workflow(&self)->&str {&self.input.workflow}
    pub(crate) fn repository(&self)->&str {&self.repository}
    pub(crate) fn recovery_binding(&self)->&crate::autoship_workflow::Binding {&self.next_binding}
    pub(crate) fn recovery_identity(&self)->Result<(String,i64,crate::autoship_workflow::OwnerPreparationBinding,String)> {
        Ok((self.input.key()?,self.activated_generation,self.preparation.clone(),self.input.body_sha256.clone()))
    }
    pub(crate) fn canonical_manifest(&self)->&str {&self.next_manifest}
    pub(crate) fn canonical_contract(&self)->&str {&self.next_binding.contract}
    pub(crate) fn request(&self)->&str {&self.input.request_id}
    pub(crate) fn generation(&self)->i64 {self.activated_generation}
    pub(crate) fn validate_current_native(&self,conn:&rusqlite::Connection,expected_audit:&Path)->Result<()> {
        use rusqlite::{params,OptionalExtension};
        let context:Option<(String,String,i64,i64)>=conn.query_row("SELECT substr(o.audit_path,1,4097),substr(b.binding,1,16385),length(CAST(o.audit_path AS BLOB)),length(CAST(b.binding AS BLOB)) FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2 AND o.retirement_reason IS NULL AND o.retired_at IS NULL",params![self.workflow(),self.activated_generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let (audit,raw,audit_bytes,binding_bytes)=context.context("owner publication activated native registration missing or retiring")?;
        ensure!((1..=4096).contains(&audit_bytes) && (1..=16384).contains(&binding_bytes),"owner publication activated native metadata exceeds bound");
        ensure!(Path::new(&audit)==expected_audit,"owner publication activated native audit changed");
        let binding:augmentagent_channel_core::native_vm::ownership::TransferBinding=serde_json::from_str(&raw)?;
        self.validate_native_binding(&binding)
    }
    fn validate_native_binding(&self,binding:&augmentagent_channel_core::native_vm::ownership::TransferBinding)->Result<()> {
        binding.validate()?;
        let sha=amendment::digest(&augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(binding)?)?);
        ensure!(sha==self.original_native.binding_sha256,"owner publication original native binding changed");Ok(())
    }
    pub(crate) fn validate_history(&self,conn:&rusqlite::Connection)->Result<()> {
        use rusqlite::{params,OptionalExtension};
        let (registration,binding,body):(String,String,String)=conn.query_row("SELECT registration,binding,owner_message_sha256 FROM workflow_owner_revision WHERE request_id=?1",[self.request()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let next:RegisteredContract=serde_json::from_str(&registration)?;self.validate_fields(&next)?;
        ensure!(serde_json::from_str::<crate::autoship_workflow::Binding>(&binding)?==self.next_binding && body==self.input.body_sha256,"owner publication activation changed");
        let prepared:String=conn.query_row("SELECT record FROM workflow_owner_preparation WHERE request_id=?1",[self.request()],|r|r.get(0)).context("owner preparation projection missing from publication")?;
        ensure!(serde_json::from_str::<crate::autoship_workflow::OwnerPreparationBinding>(&prepared)?==self.preparation,"owner publication preparation changed");
        let (before,after):(i64,i64)=conn.query_row("SELECT previous_generation,next_generation FROM workflow_owner_revision_fence WHERE request_id=?1",[self.request()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(before==self.input.generation && after==self.activated_generation,"owner publication generation fence changed");
        let request:Option<String>=conn.query_row("SELECT repository FROM command_job WHERE workflow=?1",[self.workflow()],|r|r.get(0)).optional()?;
        ensure!(request.as_deref()==Some(self.repository.as_str()),"owner publication repository changed");
        let claim:Vec<String>={let mut q=conn.prepare("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='owner_turn_claimed' LIMIT 2")?;
            let rows=q.query_map(params![self.workflow(),before],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;rows};
        ensure!(claim.len()==1,"owner publication original claim missing");let claim:serde_json::Value=serde_json::from_str(&claim[0])?;
        ensure!(claim["request_id"]==self.input.request_id && claim["pause_revision"]==self.input.pause_revision && claim["binding"]==serde_json::to_value(&self.input.binding)?,"owner publication original claim changed");
        let turn_owner:String=conn.query_row("SELECT owner FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",params![self.workflow(),self.request()],|r|r.get(0))?;
        ensure!(turn_owner==self.input.owner,"owner publication original owner changed");
        let context:Option<(String,String)>=conn.query_row("SELECT o.audit_path,b.binding FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2",params![self.workflow(),before],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (audit,binding)=context.context("owner publication original native registration missing")?;
        let binding:augmentagent_channel_core::native_vm::ownership::TransferBinding=serde_json::from_str(&binding)?;binding.validate()?;
        let sha=amendment::digest(&augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(binding)?)?);
        ensure!(audit==self.original_native.audit && sha==self.original_native.binding_sha256,"owner publication original native binding changed");Ok(())
    }
    fn validate_fields(&self,next:&RegisteredContract)->Result<()> {
        next.verify_metadata()?;
        ensure!(self.version==1 && self.input.version==1 && self.canonical.version==1 && self.canonical.source==self.input.source
            && self.input.generation>0 && self.input.generation.checked_add(1)==Some(self.activated_generation)
            && self.input.source==next.contract().source()? && self.input.revision==next.contract().revision
            && self.canonical.control.is_absolute() && self.canonical.control.to_string_lossy().len()<=4096
            && serde_json::to_value(&self.candidate)?["version"]==1 && self.next_binding.contract==next.contract_hash()
            && self.next_binding.head==self.input.binding.head && self.preparation.context_sha256()==self.input.key()?
            && self.input.body_sha256==amendment::digest(self.input.body.as_bytes()),"owner publication original input/preparation changed");
        let expected=match self.input.source {AcceptanceSource::Issue(n)=>crate::autoship_workflow::workflow_id(&self.repository,n)?,AcceptanceSource::PullRequest(n)=>crate::autoship_workflow::pr_workflow_id(&self.repository,n)?};
        ensure!(self.input.workflow==expected && self.preparation_file==self.canonical.control.join("owner-preparations").join(format!("{}.json",self.input.key()?)),"owner publication source/artifact reference changed");
        let expected_file=format!("{}-owner-{}-revision-{}-{}.json",self.input.source.key(),amendment::digest(self.input.request_id.as_bytes()),next.contract().revision,next.contract_hash());
        ensure!(self.next_evidence.file==expected_file && self.next_evidence.bytes<=4*1024*1024 && self.next_evidence.sha256.len()==64
            && self.next_evidence.sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')) && self.next_manifest.len()<=4096,"owner publication evidence exceeds bound or changed");
        let manifest:WorkflowManifest=serde_json::from_str(&self.next_manifest)?;
        ensure!(manifest.version==1 && serde_json::to_vec(&manifest)?==self.next_manifest.as_bytes() && manifest.source()?==self.input.source && manifest.original_base==next.baseline_head()
            && manifest.contract_hash==next.contract_hash() && manifest.contract_revision==next.contract().revision && manifest.evidence_file.as_deref()==Some(self.next_evidence.file.as_str()),"owner publication next manifest changed");
        match (&self.canonical.manifest,&self.input.previous_contract) {
            (None,None)=>ensure!(next.contract().revision==1,"initial owner publication revision changed"),
            (Some(raw),Some(previous))=>{
                ensure!(raw.len()<=4096,"owner publication original manifest exceeds bound");let old:WorkflowManifest=serde_json::from_str(raw)?;
                ensure!(old.version==1 && serde_json::to_value(&old)?==serde_json::from_str::<serde_json::Value>(raw)? && old.source()?==self.input.source && old.contract_hash==*previous
                    && old.original_base==next.baseline_head() && old.contract_revision.checked_add(1)==Some(next.contract().revision),"owner publication original manifest changed");
            },_=>bail!("owner publication initial/revision kind changed"),
        }
        ensure!(serde_json::to_vec(self)?.len()<=MAX_INTENT,"owner publication intent exceeds bound");Ok(())
    }
}
/// Borrowed original producer; history JSON cannot reconstruct activation input.
pub(crate) struct OwnerPublicationDraft<'a> {record:OwnerPublicationRecord,producer:&'a PreparedOwnerAcceptance}
impl OwnerPublicationDraft<'_> {
    /// Exact original retained draft join, with no filesystem or native I/O.
    pub(crate) fn require_metadata_record(&self,conn:&rusqlite::Connection,turn:&crate::autoship_workflow::owner_turn::OwnerTurn)->Result<()> {
        ensure!(turn.lease.workflow==self.record.input.workflow && turn.lease.owner==self.record.input.owner
            && turn.lease.generation==self.record.activated_generation && turn.request_id==self.record.input.request_id
            && turn.pause_revision==self.record.input.pause_revision && turn.body==self.record.input.body && turn.binding==self.record.next_binding,
            "owner publication retained metadata turn changed");
        let raw:String=conn.query_row("SELECT record FROM workflow_owner_publication_intent WHERE request_id=?1",[&turn.request_id],|r|r.get(0))?;
        ensure!(raw==serde_json::to_string(&self.record)?,"owner publication retained original intent changed");Ok(())
    }
    /// Preflight against the retained original producer. These bounded file
    /// checks precede native locks; the eventual writer must recheck its exact
    /// canonical/evidence bytes at commit, rather than treating this as a grant.
    pub(in crate::self_improve) fn metadata_turn(&self,repository:&str,lease:&crate::autoship_workflow::Lease,
        binding:&crate::autoship_workflow::Binding,candidate:&Path)->Result<crate::autoship_workflow::owner_turn::OwnerTurn> {
        ensure!(repository==self.record.repository && lease.workflow==self.record.input.workflow
            && lease.owner==self.record.input.owner && lease.generation==self.record.activated_generation
            && binding==&self.record.next_binding,"owner publication live metadata context changed");
        self.producer.candidate.verify(candidate)?;
        ensure!(self.producer.activation_binding()?==self.record.preparation,"owner publication original preparation changed");
        self.record.canonical.publication_is_next(&self.record.next_manifest)?;
        let evidence=bytes(&self.record.canonical.control.join(&self.record.next_evidence.file),4*1024*1024)?;
        ensure!(evidence.len()==self.record.next_evidence.bytes && amendment::digest(&evidence)==self.record.next_evidence.sha256
            && evidence==serde_json::to_vec(self.producer.prepared())?,"owner publication immutable next evidence changed");
        Ok(crate::autoship_workflow::owner_turn::OwnerTurn {lease:lease.clone(),request_id:self.record.input.request_id.clone(),
            body:self.record.input.body.clone(),pause_revision:self.record.input.pause_revision,binding:binding.clone()})
    }
    pub(crate) fn preparation(&self)->&crate::autoship_workflow::OwnerPreparationBinding {&self.record.preparation}
    pub(crate) fn validate_native_binding(&self,binding:&augmentagent_channel_core::native_vm::ownership::TransferBinding)->Result<()> {
        self.record.validate_native_binding(binding)
    }
    pub(crate) fn encode_for_activation(&self,turn:&crate::autoship_workflow::owner_turn::OwnerTurn,next:&RegisteredContract,
        preparation:&crate::autoship_workflow::OwnerPreparationBinding,binding:&crate::autoship_workflow::Binding)->Result<String> {
        self.record.validate_fields(next)?;
        ensure!(turn.request_id==self.record.input.request_id && turn.lease.workflow==self.record.input.workflow && turn.lease.owner==self.record.input.owner
            && turn.lease.generation==self.record.input.generation && turn.pause_revision==self.record.input.pause_revision && turn.body==self.record.input.body
            && turn.binding==self.record.input.binding && preparation==&self.record.preparation && binding==&self.record.next_binding,"owner publication original turn changed");
        self.record.canonical.verify_current()?;
        ensure!(self.producer.activation_binding()?==*preparation,"owner publication original artifact changed");
        let evidence=bytes(&self.record.canonical.control.join(&self.record.next_evidence.file),4*1024*1024)?;
        ensure!(evidence.len()==self.record.next_evidence.bytes && amendment::digest(&evidence)==self.record.next_evidence.sha256
            && evidence==serde_json::to_vec(self.producer.prepared())?,"owner publication immutable next evidence changed");
        Ok(serde_json::to_string(&self.record)?)
    }
}
pub(in crate::self_improve) fn stage<'a>(workflow:&workflow::WorkflowGuard,producer:&'a PreparedOwnerAcceptance,
    previous:Option<&RegisteredContract>,binding:&crate::autoship_workflow::Binding)->Result<OwnerPublicationDraft<'a>> {
    producer.validate_current(workflow,workflow.candidate_path(),previous)?;
    let prepared=producer.prepared();let next=&prepared.baseline.registration;let input=producer.input.clone();
    let data=serde_json::to_vec(prepared)?;ensure!(data.len()<=4*1024*1024,"owner publication next evidence exceeds bound");
    let name=format!("{}-owner-{}-revision-{}-{}.json",input.source.key(),amendment::digest(input.request_id.as_bytes()),next.contract().revision,next.contract_hash());
    let control=&producer.canonical.control;let path=control.join(&name);
    workflow.with_acceptance_metadata(|_access| {
        producer.canonical.verify_current()?;
        match std::fs::symlink_metadata(&path) {
            Ok(_)=>ensure!(bytes(&path,4*1024*1024)?==data,"owner publication next evidence is immutable"),
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{
                let mut file=tempfile::NamedTempFile::new_in(control)?;file.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file.write_all(&data)?;file.as_file().sync_all()?;file.persist_noclobber(&path).map_err(|e|e.error)?;
            },Err(e)=>return Err(e.into()),
        }
        std::fs::File::open(control)?.sync_all()?;Ok(())
    })?;
    let audit=workflow.audit_logger();let original=audit.native_store_binding()?.context("owner publication original native binding missing")?;
    let record=OwnerPublicationRecord {version:1,repository:workflow.repository().into(),input:input.clone(),candidate:producer.candidate.clone(),canonical:producer.canonical.clone(),
        preparation:producer.activation_binding()?,preparation_file:producer.path.clone(),original_native:Native {audit:audit.path().to_string_lossy().into_owned(),binding_sha256:amendment::digest(&augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(original)?)?)},
        next_binding:binding.clone(),activated_generation:input.generation.checked_add(1).context("owner publication generation overflow")?,next_manifest:{let mut m=WorkflowManifest::from_prepared(input.source,prepared);m.evidence_file=Some(name.clone());serde_json::to_string(&m)?},next_evidence:Evidence {file:name,bytes:data.len(),sha256:amendment::digest(&data)}};
    record.validate_fields(next)?;Ok(OwnerPublicationDraft {record,producer})
}
