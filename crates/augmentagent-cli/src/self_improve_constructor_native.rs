//! Untrusted constructor request/reply custody. No witness or amendment approval.
use super::*;
use std::os::unix::fs::MetadataExt;

const ROLE:&str="verifier_construction";
const LIMIT:usize=2*1024*1024;

fn policy_hash()->String {
    hash(&serde_json::to_vec(&("verifier-construction-producer-v1",hash(include_bytes!("self_improve_constructor_native.rs")),hash(include_bytes!("self_improve_amendment_source.rs")),hash(include_bytes!("self_improve_construction.rs")),
        hash(include_bytes!("../../augmentagent-channel-core/src/native_session.rs")),hash(include_bytes!("../../augmentagent-channel-core/src/codex.rs")),hash(include_bytes!("../../augmentagent-channel-core/src/reasoner.rs")))).unwrap())
}

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
    fn new(workflow:&str,original_base:&str,episode:&str,provider:augmentagent_channel_core::ProviderKind,model:&str,system:&str,prompt:&str)->Result<Self> {
        let input=Self {workflow:workflow.into(),original_base:original_base.into(),episode:episode.into(),provider:provider.name().into(),policy:policy_hash(),model:model.into(),system:system.into(),prompt:prompt.into()};
        input.validate()?;Ok(input)
    }
    pub(super) fn for_episode(episode:&amendment::ConstructionEpisode,provider:augmentagent_channel_core::ProviderKind,model:&str,system:&str,prompt:&str)->Result<Self> {
        Self::new(episode.workflow(),episode.original_base(),episode.key(),provider,model,system,prompt)
    }
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
    pub(super) fn identity(&self)->Result<String> {
        self.validate()?;
        // JSON arrays implement the ordered tuple specified by this protocol.
        let profile=serde_json::json!(["constructor-model-profile-v1",self.model,"default",false,[],[],[],null]);
        Ok(hash(&serde_json::to_vec(&("verifier-construction-input-v1",&self.episode,&self.provider,ROLE,&self.policy,profile,&self.system,&self.prompt))?))
    }
    fn purpose(&self)->Result<String> {
        Ok(format!("verifier-construction-native-v1:{}:{}",self.episode,self.identity()?))
    }
    pub(super) fn require_episode(&self,episode:&amendment::ConstructionEpisode)->Result<()> {
        self.validate()?;
        ensure!(self.workflow==episode.workflow() && self.original_base==episode.original_base()
            && self.episode==episode.key(),"constructor request differs from its controller-owned episode");
        Ok(())
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
fn completion_path(control:&Path,input:&str)->PathBuf {control.join(format!("constructor-completed-{input}.json"))}
fn response_path(control:&Path,response:&str)->PathBuf {control.join(format!("constructor-response-{response}.txt"))}

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
    let completed=Completed {version:1,role:ROLE.into(),input_sha256:input_sha256.clone(),input_file_sha256:hash(&bytes),provider:input.provider.clone(),native_session:session.into(),response_sha256:hash(response.as_bytes())};
    write_private(control,&response_path(control,&completed.response_sha256),response.as_bytes())?;
    let bytes=serde_json::to_vec(&completed)?;let artifact_sha256=hash(&bytes);
    let artifact=control.join(format!("constructor-native-{artifact_sha256}.json"));
    write_private(control,&artifact,&bytes)?;
    let reference=Reference {artifact,artifact_sha256};
    write_private(control,&completion_path(control,&input_sha256),&serde_json::to_vec(&reference)?)?;
    Ok(reference)
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
    let pointer=baseline_witness::private_bytes(&completion_path(control,&completed.input_sha256),LIMIT)?;
    ensure!(pointer==serde_json::to_vec(&reference)?,"constructor completion reference changed");
    let bytes=baseline_witness::private_bytes(&input_path(control,&completed.input_sha256),LIMIT)?;
    ensure!(hash(&bytes)==completed.input_file_sha256,"constructor dispatch intent changed");
    let input:Input=serde_json::from_slice(&bytes)?;
    let provider=match observation.provider {crate::autoship_workflow::submission::Provider::Claude=>"claude",crate::autoship_workflow::submission::Provider::Codex=>"codex"};
    ensure!(input.identity()?==completed.input_sha256 && input.workflow==workflow
        && input.provider==provider && completed.provider==provider,"constructor input/provider/workflow changed");
    let (raw,_)=crate::autoship_workflow::construction_native_response_keys(&input.purpose()?)?;
    ensure!(raw==observation.input && hash(observation.response.as_bytes())==completed.response_sha256,"constructor response differs from admitted input");
    let response=baseline_witness::private_bytes(&response_path(control,&completed.response_sha256),LIMIT)?;
    ensure!(response==observation.response.as_bytes(),"private constructor response differs from indexed observation");
    Ok(())
}

pub(super) struct Reply {pub text:String,pub provider:String,pub native_session:String}

fn original_completion(root:&Path,control:&Path,input:&Input,provider:crate::autoship_workflow::submission::Provider)->Result<(Reply,String)> {
    private_control(root,control)?;
    let encoded=baseline_witness::private_bytes(&completion_path(control,&input.identity()?),LIMIT)?;
    let reference:Reference=serde_json::from_slice(&encoded)?;
    // Validate the reference location before dereferencing it, including on a
    // crash path where the response has not yet reached the database index.
    ensure!(sha(&reference.artifact_sha256) && reference.artifact==control.join(format!("constructor-native-{}.json",reference.artifact_sha256)),"constructor recovery reference escaped authority");
    let bytes=baseline_witness::private_bytes(&reference.artifact,LIMIT)?;
    ensure!(hash(&bytes)==reference.artifact_sha256,"constructor recovery proof changed");
    let completed:Completed=serde_json::from_slice(&bytes)?;
    ensure!(sha(&completed.response_sha256),"invalid recovered constructor response identity");
    let text=String::from_utf8(baseline_witness::private_bytes(&response_path(control,&completed.response_sha256),LIMIT)?)?;
    let (raw,_)=crate::autoship_workflow::construction_native_response_keys(&input.purpose()?)?;
    let encoded=String::from_utf8(encoded)?;
    let observation=crate::autoship_workflow::NativeScopeCustody {input:raw,response:text.clone(),provenance:encoded.clone(),provider};
    validate_observation(root,control,&input.workflow,&observation)?;
    let saved=baseline_witness::private_bytes(&input_path(control,&input.identity()?),LIMIT)?;
    ensure!(saved==serde_json::to_vec(input)?,"recovered constructor input differs from saved request");
    Ok((Reply {text,provider:completed.provider,native_session:completed.native_session},encoded))
}

/// Data-only recovery of an actual completed conversation. Unlike `call`, this
/// does not require current episode enrollment and can never dispatch. Missing
/// paired publication remains a hold; ordinary leased exchange owns repair.
pub(super) fn retained(root:&Path,workflow:&workflow::WorkflowGuard,episode:&amendment::ConstructionEpisode,input:&Input)->Result<Reply> {
    (||->Result<Reply> {
        workflow.validate_before_side_effect()?;
        ensure!(workflow.id()==episode.workflow(),"constructor recovery belongs to another workflow");
        input.require_episode(episode)?;
        let purpose=input.purpose()?;
        let provider=workflow.construction_native_admission(&purpose)?.context("constructor lacks original actual admission")?;
        let (reply,reference)=original_completion(root,episode.control(),input,provider)?;
        let (raw,proof)=crate::autoship_workflow::construction_native_response_keys(&purpose)?;
        let (text,encoded)=workflow.scope_stage_response_pair(&raw,&proof)?.context("constructor completion lacks paired publication")?;
        ensure!(text==reply.text && encoded==reference,"constructor recovered data differs from paired publication");
        Ok(reply)
    })().map_err(|error|error.context(initial_repair::Hold::EvidenceUnknown))
}

#[cfg(test)]
pub(super) async fn scripted_reply(root:&Path,workflow:&workflow::WorkflowGuard,episode:&amendment::ConstructionEpisode,input:&Input,text:String)->Result<Reply> {
    let audit=workflow.audit_logger();
    exchange(root,episode.control(),workflow,input,move |provider,_,_|async move {
        let session=augmentagent_channel_core::native_session::current_for(provider)?.context("test constructor session missing")?;
        let mut native=session.begin(provider)?;
        audit.model_call_admitted(provider.name())?;
        native.observe(&format!("test-constructor-{}",uuid::Uuid::new_v4()))?;native.finish()?;
        Ok(text)
    }).await
}

/// Actual native transport. Callers still need the restricted construction
/// lease and a controller-computed episode; no proposal is approved here.
pub(super) async fn call(root:&Path,workflow:&workflow::WorkflowGuard,episode:&amendment::ConstructionEpisode,input:&Input)->Result<Reply> {
    use std::os::unix::fs::DirBuilderExt;
    episode.require_current(root,workflow)?;
    ensure!(episode.workflow()==workflow.id(),"constructor request belongs to another workflow");
    input.require_episode(episode)?;
    workflow.require_fresh_issue(root).await?;
    let control=control_directories(workflow.repository(),workflow.id())?.workflow_acceptance;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&control)?;
    exchange(root,&control,workflow,input,|provider,opts,prompt|async move {
        workflow.check_resource_policy()?;
        let reasoner=augmentagent_channel_core::build_workflow_pinned(provider).context("native constructor provider unavailable")?;
        reasoner.call(&opts,&prompt).await
    }).await
}

async fn exchange<F,Fut>(root:&Path,control:&Path,workflow:&workflow::WorkflowGuard,input:&Input,dispatch:F)->Result<Reply>
where F:FnOnce(augmentagent_channel_core::ProviderKind,augmentagent_channel_core::ReasonerOpts,String)->Fut,
    Fut:std::future::Future<Output=Result<String>> {
    use augmentagent_channel_core::{native_session,model_router,ProviderKind};
    workflow.validate_before_side_effect()?;
    input.validate()?;ensure!(input.workflow==workflow.id(),"constructor request belongs to another workflow");
    let purpose=input.purpose()?;
    let (raw,proof)=crate::autoship_workflow::construction_native_response_keys(&purpose)?;
    let indexed=workflow.scope_stage_response_pair(&raw,&proof).map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))?;
    if let Some(provider)=workflow.construction_native_admission(&purpose)? {
        // Restore only the original admitted conversation's retained output.
        // No new call, synthetic result, admission or budget credit is created.
        let (reply,encoded)=original_completion(root,control,input,provider).map_err(|e|e.context(initial_repair::Hold::EvidenceUnknown))?;
        if let Some((text,reference))=indexed {
            ensure!(text==reply.text && reference==encoded,"indexed constructor completion conflicts with original output");
        } else {workflow.record_scope_stage_response_pair(&raw,&reply.text,&proof,&encoded)?;}
        for observation in workflow.construction_native_custody()? {validate_observation(root,control,workflow.id(),&observation)?;}
        return Ok(reply);
    }
    ensure!(indexed.is_none(),"constructor response index lacks an actual admission");
    for observation in workflow.construction_native_custody()? {validate_observation(root,control,workflow.id(),&observation)?;}
    ensure!(input.policy==policy_hash(),"historical constructor input cannot authorize a new native call");
    ensure!(!completion_path(control,&input.identity()?).try_exists()?,"constructor completion lacks its original actual admission");
    let head=actual_head(root).await?;
    persist_input(root,control,input)?;
    let provider=match input.provider.as_str() {"codex"=>ProviderKind::Codex,"claude"=>ProviderKind::Claude,_=>unreachable!("validated constructor provider")};
    let mut opts=codex_review_opts(root.to_path_buf(),&input.system);
    opts.model=Some(input.model.clone());opts.allowed_tools.clear();opts.add_dirs.clear();opts.env.clear();
    opts.permission_mode="default".into();opts.restrict_env=false;opts.settings_json=None;
    opts.handoff_path=None;opts.session_id=None;opts.audit_logger=Some(workflow.audit_logger());
    let outcome=workflow.bounded_native_calls(purpose.clone(),1,native_session::fresh_review_scope(provider,
        model_router::native_reviewer_scope(provider,native_session::literal_text_scope(provider,&input.model,&input.system,&input.prompt,async {
            let text=dispatch(provider,opts,input.prompt.clone()).await?;
            ensure!(text.len()<=LIMIT && actual_head(root).await?==head,"constructor output exceeds bound or source moved");
            let session=native_session::current_for(provider)?.context("constructor has no protected native session")?;
            ensure!(!session.is_uncertain(),"constructor native outcome is uncertain");
            let native_session=session.id().context("constructor did not observe its native conversation")?;
            let reference=persist_reply(root,control,input,&native_session,&text)?;
            workflow.audit_health().await?;
            // All raw/provenance bytes and the paired index are durable before
            // fresh_review_scope releases its native conversation/runtime.
            workflow.record_scope_stage_response_pair(&raw,&text,&proof,&serde_json::to_string(&reference)?)?;
            Ok(Reply {text,provider:input.provider.clone(),native_session})
        })))).await;
    match outcome {
        Ok((reply,_))=>Ok(reply),
        Err(error) if matches!(workflow.construction_native_admission(&purpose),Ok(Some(_)))=>
            Err(error.context(initial_repair::Hold::EvidenceUnknown)),
        // Setup/capacity/quota refusals without an actual admission retain
        // their original classification and consume no conversation slot.
        Err(error)=>Err(error),
    }
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
        for mutation in ["missing-proof","missing-input","missing-response","changed-response","missing-completion","changed-input","changed-proof","exposed","symlink","worker-control"] {
            let (root,control,input,observation)=fixture();
            let reference:Reference=serde_json::from_str(&observation.provenance).unwrap();
            match mutation {
                "missing-proof"=>std::fs::remove_file(&reference.artifact).unwrap(),
                "missing-input"=>std::fs::remove_file(input_path(control.path(),&input.identity().unwrap())).unwrap(),
                "missing-response"=>std::fs::remove_file(response_path(control.path(),&hash(observation.response.as_bytes()))).unwrap(),
                "changed-response"=>std::fs::write(response_path(control.path(),&hash(observation.response.as_bytes())),b"substituted").unwrap(),
                "missing-completion"=>std::fs::remove_file(completion_path(control.path(),&input.identity().unwrap())).unwrap(),
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
    #[tokio::test]
    async fn constructor_native_exchange_replays_after_restart_and_recovers_failed_index_publication() {
        use augmentagent_channel_core::{native_session,ProviderKind};
        use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
        for mode in ["completed","index-fault","lost-output"] {
            let (root,state,_)=qualification_fixture();
            let control=state.path().join("constructor");std::fs::create_dir(&control).unwrap();
            std::fs::set_permissions(&control,std::fs::Permissions::from_mode(0o700)).unwrap();
            let path=state.path().join("workflow.db");
            let start=||workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:Some(2058),pr:None,branch:"fixture",contract:"C1",environment:"test"};
            let guard=workflow::WorkflowGuard::acquire_fixture(&path,start()).await.unwrap().unwrap();
            let input=Input::new(guard.id(),&actual_head(root.path()).await.unwrap(),&"a".repeat(64),ProviderKind::Codex,"explicit-fixture-model","Untrusted constructor; no tools","Preserve every original requirement; source data only").unwrap();
            if mode=="index-fault" {
                rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TRIGGER refuse_constructor_index BEFORE INSERT ON scope_stage_response BEGIN SELECT RAISE(ABORT,'injected index failure'); END;").unwrap();
            }
            let audit=guard.audit_logger();
            let first=exchange(root.path(),&control,&guard,&input,move |provider,opts,prompt|async move {
                assert_eq!(provider,ProviderKind::Codex);assert_eq!(opts.model.as_deref(),Some("explicit-fixture-model"));
                assert!(opts.allowed_tools.is_empty() && opts.add_dirs.is_empty() && opts.env.is_empty());
                assert!(opts.settings_json.is_none() && opts.handoff_path.is_none() && opts.session_id.is_none());
                assert_eq!(opts.permission_mode,"default");assert!(!opts.restrict_env);
                assert_eq!(prompt,"Preserve every original requirement; source data only");
                let session=native_session::current_for(provider)?.unwrap();let mut observed=session.begin(provider)?;
                audit.model_call_admitted(provider.name())?;
                observed.observe(&format!("scripted-constructor-{}",uuid::Uuid::new_v4()))?;observed.finish()?;
                if mode=="lost-output" {anyhow::bail!("original transport outcome unavailable");}
                Ok("complete original untrusted reply".into())
            }).await;
            let why=first.as_ref().err().map(|e|format!("{e:#}")).unwrap_or_default();
            assert_eq!(first.is_ok(),mode=="completed","unexpected {mode} outcome: {why}");
            if mode!="completed" {assert!(first.as_ref().err().unwrap().is::<initial_repair::Hold>());}
            let id=guard.id().to_owned();
            assert_eq!(crate::autoship_workflow::WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap().model_calls,1);
            if mode=="index-fault" {rusqlite::Connection::open(&path).unwrap().execute_batch("DROP TRIGGER refuse_constructor_index;").unwrap();}
            guard.finish().unwrap();
            let guard=workflow::WorkflowGuard::acquire_fixture(&path,start()).await.unwrap().unwrap();
            let calls=Arc::new(AtomicUsize::new(0));let observed=calls.clone();
            let replay=exchange(root.path(),&control,&guard,&input,move |_,_,_|async move {
                observed.fetch_add(1,Ordering::SeqCst);anyhow::bail!("completed or unknown conversation must not redispatch")
            }).await;
            assert_eq!(calls.load(Ordering::SeqCst),0,"replayed {mode} spent another native call");
            if mode=="lost-output" {assert!(replay.is_err());}else {
                let replay=replay.unwrap();assert_eq!(replay.text,"complete original untrusted reply");assert_eq!(replay.provider,"codex");assert!(!replay.native_session.is_empty());
                let (raw,proof)=crate::autoship_workflow::construction_native_response_keys(&input.purpose().unwrap()).unwrap();
                assert_eq!(guard.scope_stage_response_pair(&raw,&proof).unwrap().unwrap().0,replay.text);
            }
            let store=crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
            assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,1);
            assert_eq!(store.events(&id).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_attempt_started").count(),1);
            drop(store);guard.finish().unwrap();
        }
    }
    #[tokio::test]
    async fn constructor_setup_refusal_preserves_free_retry_and_spends_no_native_attempt() {
        let (root,state,_)=qualification_fixture();let control=state.path().join("constructor");
        std::fs::create_dir(&control).unwrap();std::fs::set_permissions(&control,std::fs::Permissions::from_mode(0o700)).unwrap();
        let path=state.path().join("workflow.db");
        let guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:Some(2058),pr:None,branch:"fixture",contract:"C1",environment:"test"}).await.unwrap().unwrap();
        let input=Input::new(guard.id(),&actual_head(root.path()).await.unwrap(),&"a".repeat(64),augmentagent_channel_core::ProviderKind::Codex,"fixture-model","No tools","Untrusted constructor request").unwrap();
        for _ in 0..2 {
            let error=exchange(root.path(),&control,&guard,&input,|_,_,_|async {anyhow::bail!("provider setup prerequisite unavailable")}).await.err().unwrap();
            assert!(error.to_string().contains("provider setup prerequisite unavailable"));
            assert!(!error.is::<initial_repair::Hold>(),"unadmitted setup refusal became an unknown paid conversation");
        }
        let store=crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        assert_eq!(store.events(guard.id()).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_attempt_started").count(),0);
        drop(store);guard.finish().unwrap();
    }
}
