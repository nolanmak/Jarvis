//! Untrusted constructor request/reply custody. No witness or amendment approval.
use super::*;
use std::os::unix::fs::MetadataExt;

const ROLE:&str="verifier_construction";
const LIMIT:usize=2*1024*1024;

fn hash(bytes:&[u8])->String {amendment::digest(bytes)}
fn sha(value:&str)->bool {value.len()==64 && value.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))}

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    workflow:String,
    original_base:String,
    episode:String,
    provider:String,
    policy:String,
    model:String,
    system:String,
    prompt:String,
}
impl Input {
    fn validate(&self)->Result<()> {
        ensure!(!self.workflow.is_empty() && self.workflow.len()<=1024,"invalid constructor workflow");
        ensure!(matches!(self.original_base.len(),40|64) && self.original_base.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),"invalid constructor original commit");
        ensure!(sha(&self.episode) && sha(&self.policy),"invalid constructor episode or policy");
        ensure!(matches!(self.provider.as_str(),"claude"|"codex") && !self.model.trim().is_empty() && self.model.len()<=1024,"invalid constructor provider/model");
        ensure!(!self.system.trim().is_empty() && !self.prompt.trim().is_empty()
            && self.system.len().saturating_add(self.prompt.len())<=512*1024,"constructor request exceeds bound");
        ensure!(serde_json::to_vec(self)?.len()<=LIMIT-16*1024,"serialized constructor input exceeds bound");
        Ok(())
    }
    fn identity(&self)->Result<String> {
        self.validate()?;
        // JSON arrays implement the ordered tuple specified by this protocol.
        let profile=serde_json::json!(["constructor-model-profile-v1",self.model,"default",false,[],[],[],null]);
        Ok(hash(&serde_json::to_vec(&("verifier-construction-input-v1",&self.episode,&self.provider,ROLE,&self.policy,profile,&self.system,&self.prompt))?))
    }
    fn purpose(&self)->Result<String> {
        Ok(format!("verifier-construction-native-v1:{}:{}",self.episode,self.identity()?))
    }
}

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {artifact:PathBuf,artifact_sha256:String}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Completed {
    version:u32,
    role:String,
    input_sha256:String,
    input_file_sha256:String,
    provider:String,
    native_session:String,
    response_sha256:String,
}

fn input_path(control:&Path,input:&str)->PathBuf {control.join(format!("constructor-input-{input}.json"))}

/// Same effective owner boundary as native witness evidence, with an expected
/// controller namespace supplied independently of the persisted reference.
fn private_control(root:&Path,control:&Path)->Result<()> {
    ensure!(control.is_absolute() && control.canonicalize()?==control,"constructor control namespace changed");
    let owner=unsafe{libc::geteuid()};
    ensure!(control.metadata()?.uid()==owner,"constructor control has a foreign owner");
    let mut exposed=false;let mut owned=true;let mut child_owner=owner;let mut boundary=false;
    for ancestor in control.ancestors() {
        let m=ancestor.metadata()?;
        ensure!(m.is_dir() && (m.uid()==0 || m.uid()==owner),"constructor control ancestor changed");
        if owned && m.uid()==owner && m.mode()&0o077==0 {boundary=true;break;}
        if m.mode()&0o1000!=0 && (m.uid()==0 || m.uid()==owner) && child_owner==owner && !exposed {boundary=true;break;}
        exposed|=m.mode()&0o022!=0;owned&=m.uid()==owner;child_owner=m.uid();
    }
    ensure!(boundary || !exposed,"constructor control is exposed to another writer");
    for writable in worker_roots(root) {
        if writable.exists() {ensure!(!control.starts_with(writable.canonicalize()?),"constructor control overlaps worker storage");}
    }
    Ok(())
}

fn write_private(control:&Path,path:&Path,bytes:&[u8])->Result<()> {
    use std::io::Write;
    ensure!(bytes.len()<=LIMIT && path.parent()==Some(control),"constructor artifact exceeds its namespace or bound");
    if path.try_exists()? {ensure!(baseline_witness::private_bytes(path,LIMIT)?==bytes,"constructor artifact changed");return Ok(());}
    let mut file=tempfile::NamedTempFile::new_in(control)?;
    file.write_all(bytes)?;file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|e|e.error)?;
    std::fs::File::open(control)?.sync_all()?;Ok(())
}

// The native transport must call this before actual admission. Neither the
// intent nor a later completion is proposal validation or approval.
fn persist_input(root:&Path,control:&Path,input:&Input)->Result<()> {
    private_control(root,control)?;
    write_private(control,&input_path(control,&input.identity()?),&serde_json::to_vec(input)?)
}

fn persist_reply(root:&Path,control:&Path,input:&Input,session:&str,response:&str)->Result<Reference> {
    private_control(root,control)?;
    ensure!(!session.trim().is_empty() && session.len()<=4096 && response.len()<=LIMIT,"invalid constructor completion");
    let input_sha256=input.identity()?;
    let bytes=baseline_witness::private_bytes(&input_path(control,&input_sha256),LIMIT)?;
    ensure!(bytes==serde_json::to_vec(input)?,"constructor completion differs from dispatch intent");
    let completed=Completed {version:1,role:ROLE.into(),input_sha256,input_file_sha256:hash(&bytes),provider:input.provider.clone(),native_session:session.into(),response_sha256:hash(response.as_bytes())};
    let bytes=serde_json::to_vec(&completed)?;let artifact_sha256=hash(&bytes);
    let artifact=control.join(format!("constructor-native-{artifact_sha256}.json"));
    write_private(control,&artifact,&bytes)?;
    Ok(Reference {artifact,artifact_sha256})
}

fn validate_observation(root:&Path,control:&Path,workflow:&str,observation:&crate::autoship_workflow::NativeScopeCustody)->Result<()> {
    private_control(root,control)?;
    let reference:Reference=serde_json::from_str(&observation.provenance)?;
    ensure!(sha(&reference.artifact_sha256) && reference.artifact==control.join(format!("constructor-native-{}.json",reference.artifact_sha256)),"constructor provenance escaped its controller namespace");
    let bytes=baseline_witness::private_bytes(&reference.artifact,LIMIT)?;
    ensure!(hash(&bytes)==reference.artifact_sha256,"constructor provenance changed");
    let completed:Completed=serde_json::from_slice(&bytes)?;
    ensure!(completed.version==1 && completed.role==ROLE && sha(&completed.input_sha256)
        && !completed.native_session.trim().is_empty() && completed.native_session.len()<=4096,"constructor provenance lacks native ownership");
    let bytes=baseline_witness::private_bytes(&input_path(control,&completed.input_sha256),LIMIT)?;
    ensure!(hash(&bytes)==completed.input_file_sha256,"constructor dispatch intent changed");
    let input:Input=serde_json::from_slice(&bytes)?;
    let provider=match observation.provider {crate::autoship_workflow::submission::Provider::Claude=>"claude",crate::autoship_workflow::submission::Provider::Codex=>"codex"};
    ensure!(input.identity()?==completed.input_sha256 && input.workflow==workflow
        && input.provider==provider && completed.provider==provider,"constructor input/provider/workflow changed");
    let (raw,_)=crate::autoship_workflow::construction_native_response_keys(&input.purpose()?)?;
    ensure!(raw==observation.input && hash(observation.response.as_bytes())==completed.response_sha256,"constructor response differs from admitted input");
    Ok(())
}

pub(super) fn require_retained(root:&Path,repository:&str,workflow:&str,observations:&[crate::autoship_workflow::NativeScopeCustody])->Result<()> {
    if observations.is_empty() {return Ok(());}
    (||->Result<()> {
        let control=control_directories(repository,workflow)?.workflow_acceptance;
        for observation in observations {validate_observation(root,&control,workflow,observation)?;}
        Ok(())
    })().map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn constructor_completed_output_is_durable_before_response_index_publication() {
        let (root,control,input,observation)=fixture();
        // The fixture deliberately has no response-index database. Completion
        // must retain the original raw bytes independently before publication.
        let path=control.path().join(format!("constructor-response-{}.txt",hash(observation.response.as_bytes())));
        assert_eq!(std::fs::read(path).ok().as_deref(),Some(observation.response.as_bytes()),
            "completed native output disappears if response-index publication fails");
        validate_observation(root.path(),control.path(),&input.workflow,&observation).unwrap();
    }
    fn fixture()->(tempfile::TempDir,tempfile::TempDir,Input,crate::autoship_workflow::NativeScopeCustody) {
        let root=tempfile::tempdir().unwrap();
        let control=tempfile::Builder::new().prefix("constructor-private-").tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(control.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        let input=Input {workflow:"owner/repo#2058".into(),original_base:"a".repeat(40),episode:"b".repeat(64),provider:"codex".into(),policy:"c".repeat(64),model:"explicit-test-model".into(),system:"Untrusted constructor; no tools".into(),prompt:"Complete unchanged owner requirements and original source data".into()};
        persist_input(root.path(),control.path(),&input).unwrap();
        let response="untrusted completed constructor data";
        let reference=persist_reply(root.path(),control.path(),&input,"actual-session-fixture",response).unwrap();
        let (raw,_)=crate::autoship_workflow::construction_native_response_keys(&input.purpose().unwrap()).unwrap();
        let observation=crate::autoship_workflow::NativeScopeCustody {input:raw,response:response.into(),provenance:serde_json::to_string(&reference).unwrap(),provider:crate::autoship_workflow::submission::Provider::Codex};
        (root,control,input,observation)
    }
    #[test]
    fn constructor_physical_custody_accepts_exact_completed_data_and_rejects_substitution() {
        let (root,control,input,mut observation)=fixture();
        validate_observation(root.path(),control.path(),&input.workflow,&observation).unwrap();
        assert!(validate_observation(root.path(),control.path(),"owner/repo#other",&observation).is_err());
        observation.provider=crate::autoship_workflow::submission::Provider::Claude;
        assert!(validate_observation(root.path(),control.path(),&input.workflow,&observation).is_err());
        observation.provider=crate::autoship_workflow::submission::Provider::Codex;
        observation.response.push_str(" changed");
        assert!(validate_observation(root.path(),control.path(),&input.workflow,&observation).is_err());
    }
    #[test]
    fn constructor_physical_custody_rejects_missing_changed_exposed_or_linked_evidence() {
        for mutation in ["missing-proof","missing-input","changed-input","changed-proof","exposed","symlink","worker-control"] {
            let (root,control,input,observation)=fixture();
            let reference:Reference=serde_json::from_str(&observation.provenance).unwrap();
            match mutation {
                "missing-proof"=>std::fs::remove_file(&reference.artifact).unwrap(),
                "missing-input"=>std::fs::remove_file(input_path(control.path(),&input.identity().unwrap())).unwrap(),
                "changed-input"=>std::fs::write(input_path(control.path(),&input.identity().unwrap()),b"{}").unwrap(),
                "changed-proof"=>std::fs::write(&reference.artifact,b"{}").unwrap(),
                "exposed"=>std::fs::set_permissions(&reference.artifact,std::fs::Permissions::from_mode(0o644)).unwrap(),
                "symlink"=>{let bytes=std::fs::read(&reference.artifact).unwrap();std::fs::remove_file(&reference.artifact).unwrap();let other=control.path().join("other");std::fs::write(&other,bytes).unwrap();std::os::unix::fs::symlink(other,&reference.artifact).unwrap();},
                "worker-control"=>{assert!(validate_observation(control.path(),control.path(),&input.workflow,&observation).is_err());continue;},
                _=>unreachable!(),
            }
            assert!(validate_observation(root.path(),control.path(),&input.workflow,&observation).is_err(),"accepted {mutation}");
        }
    }
    #[test]
    fn constructor_input_identity_binds_native_configuration_and_intent_is_immutable() {
        let (root,control,input,_)=fixture();let first=input.identity().unwrap();
        for field in ["provider","model","policy","system","prompt"] {
            let mut changed=input.clone();
            match field {"provider"=>changed.provider="claude".into(),"model"=>changed.model.push('2'),"policy"=>changed.policy="d".repeat(64),"system"=>changed.system.push('2'),"prompt"=>changed.prompt.push('2'),_=>unreachable!()}
            assert_ne!(changed.identity().unwrap(),first);
        }
        let mut changed=input.clone();changed.workflow="owner/repo#foreign".into();
        assert!(persist_input(root.path(),control.path(),&changed).is_err(),"same exact identity reassigned its protected workflow");
        assert!(persist_reply(root.path(),control.path(),&input,"", "data").is_err());
    }
}
