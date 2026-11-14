//! Observe failed setup dependencies, never grant execution or publish setup.
use super::*;
use crate::autoship_workflow::{RetainedSetupDependency,RetainedSetupCapacity};
use std::os::unix::{ffi::OsStrExt,fs::MetadataExt};
#[path="self_improve_setup_quota.rs"]
mod quota;

#[derive(Debug)]
struct FailedSetup(RetainedSetupDependency);
impl std::fmt::Display for FailedSetup {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"retained {} setup is unavailable ({})",self.0.operation,self.0.condition)}
}
impl std::error::Error for FailedSetup {}
#[derive(Debug)]
struct FailedCandidate(crate::autoship_workflow::Binding);
impl std::fmt::Display for FailedCandidate {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"owned failed setup inputs observed")}
}
impl std::error::Error for FailedCandidate {}
pub(super) fn candidate(error:&anyhow::Error)->Option<&crate::autoship_workflow::Binding> {error.downcast_ref::<FailedCandidate>().map(|candidate|&candidate.0)}
pub(super) fn stamp(error:anyhow::Error,binding:crate::autoship_workflow::Binding)->anyhow::Error {error.context(FailedCandidate(binding))}

pub(super) fn failure(error:&anyhow::Error)->Option<&RetainedSetupDependency> {
    error.downcast_ref::<FailedSetup>().map(|failure|&failure.0)
}

pub(super) fn run<T>(workflow:&workflow::WorkflowGuard,root:&Path,control:&Path,prepared:&PreparedAcceptance,
    operation:&str,execute:impl FnOnce()->Result<T>)->Result<T> {
    match execute() {
        Ok(value)=>Ok(value),
        Err(error)=>{
            if workflow.retained_qualification_reference(prepared)?.is_none() {return Err(error);}
            Err(capture(root,control,prepared,operation,error))
        },
    }
}

pub(super) fn capture(root:&Path,control:&Path,prepared:&PreparedAcceptance,operation:&str,error:anyhow::Error)->anyhow::Error {
            let Some(io)=error.downcast_ref::<std::io::Error>() else{return error};
            let condition=match io.raw_os_error() {
                Some(libc::ENOSPC|libc::EDQUOT)=>"capacity",
                Some(libc::EACCES|libc::EPERM|libc::EROFS)=>"permission",
                _=>"other",
            };
            let version=if io.raw_os_error()==Some(libc::EDQUOT) {2}else{1};
            let observed=observe(root,control,prepared,operation,condition,version).unwrap_or_else(|_|RetainedSetupDependency {
                version,operation:operation.into(),condition:condition.into(),fingerprint:None,capacity:None,quotas:None,completed:false,
            });
            error.context(FailedSetup(observed))
}

pub(super) fn observe(root:&Path,control:&Path,prepared:&PreparedAcceptance,operation:&str,condition:&str,version:u32)->Result<RetainedSetupDependency> {
    let targets=match operation {
        "prepared"=>{
            let source=prepared.proposal.contract.source()?;
            let index=control.join(format!("{}-workflow.json",source.key()));
            let manifest=if index.try_exists()? {Some(private_json::<WorkflowManifest>(&index)?)}else{None};
            vec![prepared_destination(control,source,prepared.baseline.registration.baseline_head(),manifest.as_ref())?]
        },
        "manifest"=>vec![control.join(format!("{}-workflow.json",prepared.proposal.contract.source()?.key()))],
        "qualification"=>vec![control.to_path_buf()],
        "harness"=>{
            ensure!(prepared.proposal.files.len()<=256,"setup pin observation exceeds bound");
            prepared.proposal.files.keys().map(|relative| {
                ensure!(harness_path(relative),"setup observation has an invalid harness path");Ok(root.join(relative))
            }).collect::<Result<Vec<_>>>()?
        },
        "registration"=>{
            let (allowed,pinned)=write_admission_inputs(prepared);
            vec![augmentagent_channel_core::autoship_write_scope::registration_publication(root,
                prepared.baseline.registration.contract_hash(),allowed,pinned)?.path]
        },
        _=>bail!("unsupported retained setup operation"),
    };
    let roles=if operation=="qualification" {vec![("@qualification",baseline_witness::qualification_destination(control,prepared)?)]}else{vec![]};
    snapshot_with_roles(operation,condition,&targets,&roles,version)
}

pub(super) fn completed(root:&Path,control:&Path,prepared:&PreparedAcceptance,operation:&str)->Result<bool> {
    let source=prepared.proposal.contract.source()?;
    match operation {
        "harness"=>{
            let root=root.canonicalize()?;
            prepared.baseline.registration.verify_source_files(&root,&prepared.proposal.files)?;
            for (path,body) in &prepared.proposal.files {ensure!(harness_path(path),"invalid harness completion path");if !existing_harness(&root.join(path),body.as_bytes())? {return Ok(false);}}
            Ok(true)
        },
        "registration"=>{
            let (allowed,pinned)=write_admission_inputs(prepared);
            Ok(augmentagent_channel_core::autoship_write_scope::registration_publication(root,prepared.baseline.registration.contract_hash(),allowed,pinned)?.registered)
        },
        "prepared"=>match retained_prepared(root,control,source)? {
            Some(actual)=>Ok(serde_json::to_vec(&actual)?==serde_json::to_vec(prepared)?),None=>Ok(false),
        },
        "manifest"=>{
            let index=control.join(format!("{}-workflow.json",source.key()));if !index.try_exists()? {return Ok(false);}
            let actual:WorkflowManifest=private_json(&index)?;validate_retained_manifest(control,&actual,source)?;
            let mut expected=WorkflowManifest::from_prepared(source,prepared);expected.evidence_file=actual.evidence_file.clone();
            Ok(actual==expected)
        },
        "qualification"=>Ok(baseline_witness::assert_current(root,control,prepared).is_ok()),
        _=>bail!("unsupported setup completion observation"),
    }
}

#[derive(Serialize)]
struct Access {uid:u32,gid:u32,groups:Vec<u32>}
#[derive(Serialize)]
struct Node {dev:u64,ino:u64,uid:u32,gid:u32,mode:u32,size:Option<u64>,labels:Vec<(String,Option<String>)>}
struct Space {flags:u64,capacity:Option<RetainedSetupCapacity>}
#[derive(Default)]
struct Budget {label_bytes:usize,entries:usize,name_bytes:usize}

#[cfg(target_os="linux")]
fn labels(path:&Path,budget:&mut Budget)->Result<Vec<(String,Option<String>)>> {
    let path=std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut labels=Vec::new();
    for name in ["system.posix_acl_access","system.posix_acl_default","security.selinux"] {
        let cname=std::ffi::CString::new(name)?;
        let length=unsafe{libc::lgetxattr(path.as_ptr(),cname.as_ptr(),std::ptr::null_mut(),0)};
        if length<0 {
            let error=std::io::Error::last_os_error();
            if matches!(error.raw_os_error(),Some(libc::ENODATA|libc::ENOTSUP)) {labels.push((name.into(),None));continue;}
            return Err(error.into());
        }
        let length=length as usize;budget.label_bytes+=length;
        ensure!(length<=16*1024 && budget.label_bytes<=8*1024*1024,"setup security metadata exceeds bound");
        let mut bytes=vec![0u8;length];
        let read=unsafe{libc::lgetxattr(path.as_ptr(),cname.as_ptr(),bytes.as_mut_ptr().cast(),length)};
        if read<0 {return Err(std::io::Error::last_os_error().into());}
        ensure!(read as usize==length,"setup security metadata changed while reading");
        labels.push((name.into(),Some(amendment::digest(&bytes))));
    }
    Ok(labels)
}
#[cfg(not(target_os="linux"))]
fn labels(_: &Path,_:&mut Budget)->Result<Vec<(String,Option<String>)>> {bail!("setup dependency observation unavailable on this platform")}

fn node(path:&Path,meta:&std::fs::Metadata,budget:&mut Budget)->Result<Node> {
    ensure!(!meta.file_type().is_symlink(),"setup dependency crosses a redirected path");
    Ok(Node {dev:meta.dev(),ino:meta.ino(),uid:meta.uid(),gid:meta.gid(),mode:meta.mode(),
        size:meta.is_file().then_some(meta.len()),labels:labels(path,budget)?})
}

fn space(path:&Path,capacity:bool,budget:&mut Budget)->Result<Space> {
    let cpath=std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut raw=std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe{libc::statvfs(cpath.as_ptr(),raw.as_mut_ptr())}<0 {return Err(std::io::Error::last_os_error().into());}
    let raw=unsafe{raw.assume_init()};
    let observed_capacity=if capacity {
        let (mut entries,mut name_bytes)=(0u64,0u64);
        for entry in std::fs::read_dir(path)? {
            let name=entry?.file_name().as_bytes().to_vec();budget.entries+=1;budget.name_bytes+=name.len();
            ensure!(budget.entries<=8192 && budget.name_bytes<=4*1024*1024,"setup entry observation exceeds bound");
            if [b".jarvis-recovery-publication-stage".as_slice(),b".jarvis-recovery-publication-final".as_slice()].contains(&name.as_slice()) {continue;}
            entries+=1;name_bytes+=name.len() as u64;
        }
        Some(RetainedSetupCapacity {available_blocks:raw.f_bavail as u64,available_inodes:raw.f_favail as u64,
            block_size:raw.f_frsize as u64,entries,name_bytes})
    }else{None};
    Ok(Space {flags:raw.f_flag as u64 & libc::ST_RDONLY as u64,capacity:observed_capacity})
}

fn encode(operation:&str,condition:&str,access:Access,targets:&[String],nodes:BTreeMap<String,Option<Node>>,spaces:BTreeMap<String,u64>)->Result<Vec<u8>> {
    let bytes=serde_json::to_vec(&("retained-setup-filesystem-dependencies-v1",operation,condition,access,targets,nodes,spaces))?;
    ensure!(bytes.len()<=4*1024*1024,"setup dependency encoding exceeds bound");Ok(bytes)
}

#[cfg(test)]
fn snapshot(operation:&str,condition:&str,targets:&[PathBuf])->Result<RetainedSetupDependency> {
    snapshot_with_roles(operation,condition,targets,&[],1)
}
fn snapshot_with_roles(operation:&str,condition:&str,targets:&[PathBuf],roles:&[(&str,PathBuf)],version:u32)->Result<RetainedSetupDependency> {
    ensure!(version==1 || version==2 && condition=="capacity","unsupported setup observation version");
    ensure!(["prepared","manifest","harness","registration","qualification"].contains(&operation),"unknown setup operation");
    ensure!(["permission","capacity","other"].contains(&condition) && targets.len()<=256,"invalid setup observation");
    ensure!(!targets.is_empty(),"setup observation has no namespace");
    let mut targets=targets.iter().map(|target|Ok(target.to_str().context("setup path is not UTF8")?.to_owned())).collect::<Result<Vec<String>>>()?;
    targets.sort();targets.dedup();
    let mut nodes=BTreeMap::<String,Option<Node>>::new();let mut spaces=BTreeMap::<String,u64>::new();let mut capacities=BTreeMap::new();let mut quotas=BTreeMap::new();let mut budget=Budget::default();
    for target in &targets {
        let target=Path::new(target);
        ensure!(target.is_absolute() && target.components().all(|part|matches!(part,std::path::Component::RootDir|std::path::Component::Normal(_))),"setup observation needs a literal absolute path");
        let mut prefix=PathBuf::new();
        for part in target.components() {
            prefix.push(part);let key=prefix.to_str().context("setup path is not UTF8")?.to_owned();
            if nodes.contains_key(&key) {continue;}
            ensure!(nodes.len()<8192,"setup path observation exceeds bound");
            let observed=match std::fs::symlink_metadata(&prefix) {
                Ok(meta)=>Some(node(&prefix,&meta,&mut budget)?),
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>None,
                Err(error)=>return Err(error.into()),
            };
            nodes.insert(key,observed);
        }
        let mut directory=target;
        loop {
            match std::fs::symlink_metadata(directory) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink()=>break,
                Ok(meta) if meta.file_type().is_symlink()=>bail!("setup namespace is redirected"),
                Ok(_)=>{},
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
                Err(error)=>return Err(error.into()),
            }
            directory=directory.parent().context("setup namespace parent missing")?;
        }
        let key=directory.to_str().context("setup directory is not UTF8")?.to_owned();
        if !spaces.contains_key(&key) {
            let observed=space(directory,condition=="capacity",&mut budget)?;
            if let Some(capacity)=observed.capacity {
                let id=amendment::digest(&serde_json::to_vec(&("retained-setup-capacity-namespace-v1",&key))?);
                capacities.insert(id,capacity);
            }
            if version==2 {
                for (kind,value) in quota::observe(directory)?.into_iter().enumerate() {
                    let id=amendment::digest(&serde_json::to_vec(&("retained-setup-quota-namespace-v1",&key,kind))?);
                    quotas.insert(id,value);
                }
            }
            spaces.insert(key,observed.flags);
        }
    }
    ensure!(roles.is_empty() || operation=="qualification" && roles.len()==1 && roles[0].0=="@qualification","unknown setup target role");
    for (role,path) in roles {
        let observed=match std::fs::symlink_metadata(path) {
            Ok(meta)=>Some(node(path,&meta,&mut budget)?),
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>None,
            Err(error)=>return Err(error.into()),
        };
        nodes.insert((*role).into(),observed);
    }
    let count=unsafe{libc::getgroups(0,std::ptr::null_mut())};ensure!((0..=256).contains(&count),"setup access groups unavailable or exceed bound");
    let mut groups=vec![0 as libc::gid_t;count as usize];
    if unsafe{libc::getgroups(count,groups.as_mut_ptr())}!=count {bail!("setup access groups changed");}
    groups.sort();groups.dedup();
    let access=Access {uid:unsafe{libc::geteuid()},gid:unsafe{libc::getegid()},groups:groups.into_iter().map(|group|group as u32).collect()};
    let bytes=encode(operation,condition,access,&targets,nodes,spaces)?;
    Ok(RetainedSetupDependency {version,operation:operation.into(),condition:condition.into(),fingerprint:Some(amendment::digest(&bytes)),
        capacity:(condition=="capacity").then_some(capacities),quotas:(version==2).then_some(quotas),completed:false})
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn quota_exhaustion_capture_is_distinct_from_filesystem_exhaustion() {
        let (root,control,prepared)=baseline_witness::prepared_fixture();
        for (errno,version) in [(libc::EDQUOT,2),(libc::ENOSPC,1)] {
            let error=capture(root.path(),control.path(),&prepared,"prepared",std::io::Error::from_raw_os_error(errno).into());
            assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().raw_os_error(),Some(errno));
            let setup=failure(&error).unwrap();assert_eq!(setup.version,version,"quota exhaustion was recorded as global disk exhaustion");
            assert_eq!(setup.condition,"capacity");assert!(!setup.completed);
        }
    }
    #[test]
    fn setup_fingerprint_encoding_keeps_a_frozen_data_vector_without_code_or_clock() {
        let access=Access {uid:1000,gid:1000,groups:vec![1000]};
        let nodes=BTreeMap::from([("/fixture".into(),Some(Node {dev:7,ino:9,uid:1000,gid:1000,mode:16832,size:None,
            labels:vec![("system.posix_acl_access".into(),None),("system.posix_acl_default".into(),None),("security.selinux".into(),None)]}))]);
        let bytes=encode("harness","permission",access,&["/fixture/case.py".into()],nodes,BTreeMap::from([("/fixture".into(),0)])).unwrap();
        let expected=r#"["retained-setup-filesystem-dependencies-v1","harness","permission",{"uid":1000,"gid":1000,"groups":[1000]},["/fixture/case.py"],{"/fixture":{"dev":7,"ino":9,"uid":1000,"gid":1000,"mode":16832,"size":null,"labels":[["system.posix_acl_access",null],["system.posix_acl_default",null],["security.selinux",null]]}},{"/fixture":0}]"#;
        assert_eq!(bytes,expected.as_bytes());
        assert_eq!(amendment::digest(&bytes),"a8be73b4c4cb6e0c50b1c71010115fe035095fa13bc287d642769d9cb17aa6de");
    }
    #[test]
    fn permission_failure_ignores_probe_churn_and_unrelated_capacity_but_observes_access_repair() {
        let dir=tempfile::tempdir().unwrap();let root=dir.path();std::fs::set_permissions(root,std::fs::Permissions::from_mode(0o700)).unwrap();
        let targets=vec![root.join("missing-harness.py")];let before=snapshot("harness","permission",&targets).unwrap();
        recovery_publication::probe(root,&root.join("receipt.json"),b"expected",64).unwrap();
        std::fs::write(root.join("unrelated-build-output"),vec![1u8;128*1024]).unwrap();
        assert_eq!(snapshot("harness","permission",&targets).unwrap(),before,"inspector/capacity churn manufactured a permission change");
        std::fs::set_permissions(root,std::fs::Permissions::from_mode(0o750)).unwrap();
        assert_ne!(snapshot("harness","permission",&targets).unwrap(),before,"real access condition repair remained invisible");
    }
    #[test]
    fn capacity_observation_includes_its_directory_entries_and_missing_parent_repair() {
        let dir=tempfile::tempdir().unwrap();let targets=vec![dir.path().join("missing/case.py")];
        let before=snapshot("harness","capacity",&targets).unwrap();
        std::fs::create_dir(dir.path().join("missing")).unwrap();
        assert_ne!(snapshot("harness","capacity",&targets).unwrap(),before);
        let mut budget=Budget::default();let a=space(dir.path(),true,&mut budget).unwrap();
        let stable=snapshot("harness","capacity",&targets).unwrap().fingerprint;
        std::fs::write(dir.path().join("new-entry"),b"").unwrap();let b=space(dir.path(),true,&mut budget).unwrap();
        assert!(b.capacity.as_ref().unwrap().entries>a.capacity.as_ref().unwrap().entries,"capacity omitted directory-entry dependencies");
        assert_eq!(snapshot("harness","capacity",&targets).unwrap().fingerprint,stable,"capacity counters polluted namespace identity");
    }
    #[test]
    fn redirected_and_nonliteral_namespaces_cannot_supply_complete_proof() {
        let dir=tempfile::tempdir().unwrap();let outside=tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(),dir.path().join("redirect")).unwrap();
        assert!(snapshot("harness","permission",&[dir.path().join("redirect/new.py")]).is_err());
        assert!(snapshot("harness","permission",&[dir.path().join("../new.py")]).is_err());
        assert!(snapshot("unknown","permission",&[dir.path().into()]).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }
    #[test]
    fn completion_observes_real_writer_state_without_publishing_or_repairing_pins() {
        let (root,control,prepared)=baseline_witness::prepared_fixture();let source=prepared.proposal.contract.source().unwrap();
        let harness=root.path().join(prepared.proposal.files.keys().next().unwrap());std::fs::remove_file(&harness).unwrap();
        std::fs::remove_dir(harness.parent().unwrap()).unwrap();
        assert!(!completed(root.path(),control.path(),&prepared,"harness").unwrap());
        assert!(!harness.parent().unwrap().exists(),"completion created a harness parent");
        assert!(!completed(root.path(),control.path(),&prepared,"prepared").unwrap());
        assert!(!completed(root.path(),control.path(),&prepared,"manifest").unwrap());
        install_harness(root.path(),&prepared.proposal).unwrap();assert!(completed(root.path(),control.path(),&prepared,"harness").unwrap());
        let path=evidence_path(control.path(),source,prepared.baseline.registration.baseline_head()).unwrap();
        persist_initial_prepared(control.path(),&path,&prepared).unwrap();
        assert!(completed(root.path(),control.path(),&prepared,"prepared").unwrap());
        assert!(!completed(root.path(),control.path(),&prepared,"manifest").unwrap());
        assert!(!control.path().join(format!("{}-workflow.json",source.key())).exists(),"completion recovered an orphan by writing a manifest");
        write_manifest(control.path(),&WorkflowManifest::from_prepared(source,&prepared)).unwrap();
        assert!(completed(root.path(),control.path(),&prepared,"manifest").unwrap());
        assert!(!completed(root.path(),control.path(),&prepared,"qualification").unwrap());
        baseline_witness::persist_scripted(root.path(),control.path(),&prepared,true).unwrap();
        assert!(completed(root.path(),control.path(),&prepared,"qualification").unwrap());
        std::fs::write(&harness,b"changed verifier\n").unwrap();
        assert!(completed(root.path(),control.path(),&prepared,"harness").is_err());
        assert_eq!(std::fs::read(&harness).unwrap(),b"changed verifier\n");
    }
    #[test]
    fn actual_receipt_repairs_are_observed_without_encoding_a_policy_filename() {
        let (root,control,prepared)=baseline_witness::prepared_fixture();
        let before=observe(root.path(),control.path(),&prepared,"qualification","permission",1).unwrap();
        let receipt=baseline_witness::qualification_destination(control.path(),&prepared).unwrap();
        std::fs::write(&receipt,b"receipt requiring repair").unwrap();
        let present=observe(root.path(),control.path(),&prepared,"qualification","permission",1).unwrap();assert_ne!(before,present);
        std::fs::remove_file(&receipt).unwrap();
        assert_eq!(observe(root.path(),control.path(),&prepared,"qualification","permission",1).unwrap(),before);
    }
}
