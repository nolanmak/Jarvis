//! Read-only verification of the controller's native guest inventory.
use super::*;

#[derive(Debug, Serialize)]
pub struct RuntimeReport {
    pub status: String,
    pub policy_sha256: Option<String>,
    pub guest_manifest_sha256: Option<String>,
    pub build_status: String,
    pub build_config_sha256: Option<String>,
    pub build_artifact_sha256: BTreeMap<String,String>,
    pub verified_files: usize,
    pub verified_bytes: u64,
    pub live_verified: bool,
}

pub fn inspect() -> RuntimeReport {
    let path=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    inspect_at(path.as_deref().map(Path::new))
}

pub fn inspect_at(path:Option<&Path>) -> RuntimeReport {
    use sha2::{Digest,Sha256};
    let mut report=RuntimeReport {status:"policy-missing".into(),policy_sha256:None,guest_manifest_sha256:None,
        build_status:"unverified".into(),build_config_sha256:None,build_artifact_sha256:BTreeMap::new(),
        verified_files:0,verified_bytes:0,live_verified:false};
    let Some(path)=path else{return report};
    let bytes=match private(path) {Ok(bytes)=>bytes,Err(_)=>{report.status="policy-invalid".into();return report}};
    report.policy_sha256=Some(format!("{:x}",Sha256::digest(&bytes)));
    let policy=match serde_json::from_slice::<Policy>(&bytes) {Ok(policy) if policy.version==1=>policy,
        _=>{report.status="policy-invalid".into();return report}};
    match verify_build_config(&policy.runtime_config) {
        Ok((config,artifacts))=>{report.build_status="build-declaration-verified".into();report.build_config_sha256=Some(config);report.build_artifact_sha256=artifacts;},
        Err(_)=>report.build_status=if policy.runtime_config.exists() {"build-runtime-invalid"} else {"build-runtime-missing"}.into(),
    }
    match verify_guest(&policy.guest_runtime) {
        Ok((manifest,files,bytes))=>{
            report.status="native-guest-verified".into();report.guest_manifest_sha256=Some(manifest);
            report.verified_files=files;report.verified_bytes=bytes;
        },
        Err(reason)=>report.status=reason.into(),
    }
    report
}

fn verify_guest(root:&Path)->std::result::Result<(String,usize,u64),&'static str> {
    use sha2::{Digest,Sha256};
    use std::collections::BTreeSet;
    let metadata=std::fs::symlink_metadata(root).map_err(|_|"guest-runtime-missing")?;
    if !root.is_absolute() || !metadata.is_dir() || metadata.uid()!=unsafe{libc::geteuid()} || metadata.mode() & 0o077!=0 {
        return Err("guest-runtime-not-private");
    }
    let manifest=root.join("manifest.json");
    let mut file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&manifest).map_err(|_|"guest-manifest-missing")?;
    let meta=file.metadata().map_err(|_|"guest-manifest-invalid")?;
    if !meta.is_file() || meta.len()>2*1024*1024 {return Err("guest-manifest-invalid");}
    let mut bytes=Vec::new();
    use std::io::Read;
    (&mut file).take(2*1024*1024+1).read_to_end(&mut bytes).map_err(|_|"guest-manifest-invalid")?;
    if bytes.len()>2*1024*1024 {return Err("guest-manifest-invalid");}
    let value:serde_json::Value=serde_json::from_slice(&bytes).map_err(|_|"guest-manifest-invalid")?;
    if value["version"]!=1 {return Err("guest-manifest-invalid");}
    let entries=value["files"].as_array().filter(|entries|!entries.is_empty() && entries.len()<=10000).ok_or("guest-manifest-invalid")?;
    let mut declared=BTreeSet::new();
    let mut total=0u64;
    for entry in entries {
        let relative=entry["path"].as_str().ok_or("guest-manifest-invalid")?;
        if relative.split('/').any(|part|part.is_empty() || part=="." || part=="..") || relative=="manifest.json"
            || !declared.insert(relative.to_owned()) {return Err("guest-manifest-invalid");}
        let expected=entry["sha256"].as_str().filter(|sha|sha.len()==64 && sha.bytes().all(|b|b.is_ascii_hexdigit())).ok_or("guest-manifest-invalid")?;
        let mut path=root.to_path_buf();
        for part in relative.split('/') {
            path.push(part);
            if std::fs::symlink_metadata(&path).map_err(|_|"guest-artifact-missing")?.file_type().is_symlink() {
                return Err("guest-artifact-link");
            }
        }
        let mut artifact=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path).map_err(|_|"guest-artifact-missing")?;
        let before=artifact.metadata().map_err(|_|"guest-artifact-invalid")?;
        if !before.is_file() || before.len()>512*1024*1024 {return Err("guest-artifact-invalid");}
        total=total.checked_add(before.len()).filter(|bytes|*bytes<=8*1024*1024*1024).ok_or("guest-inventory-exceeds-bound")?;
        let mut digest=Sha256::new();let mut buffer=[0u8;65536];let mut read=0u64;
        loop {
            let count=artifact.read(&mut buffer).map_err(|_|"guest-artifact-unreadable")?;
            if count==0 {break;}
            read+=count as u64;if read>before.len() {return Err("guest-artifact-changed");}
            digest.update(&buffer[..count]);
        }
        let after=artifact.metadata().map_err(|_|"guest-artifact-unreadable")?;
        if read!=before.len() || before.mtime()!=after.mtime() || before.mtime_nsec()!=after.mtime_nsec()
            || before.ctime()!=after.ctime() || before.ctime_nsec()!=after.ctime_nsec() {
            return Err("guest-artifact-changed");
        }
        if format!("{:x}",digest.finalize())!=expected {return Err("guest-artifact-digest-mismatch");}
    }
    fn inventory(root:&Path,at:&Path,depth:usize,files:&mut BTreeSet<String>)->std::result::Result<(),&'static str> {
        if depth>128 {return Err("guest-inventory-exceeds-bound");}
        for entry in std::fs::read_dir(at).map_err(|_|"guest-inventory-unreadable")? {
            let entry=entry.map_err(|_|"guest-inventory-unreadable")?;
            let kind=entry.file_type().map_err(|_|"guest-inventory-unreadable")?;
            if kind.is_symlink() {return Err("guest-artifact-link");}
            if kind.is_dir() {inventory(root,&entry.path(),depth+1,files)?;}
            else if kind.is_file() {
                files.insert(entry.path().strip_prefix(root).map_err(|_|"guest-inventory-invalid")?.to_str().ok_or("guest-inventory-invalid")?.into());
                if files.len()>10001 {return Err("guest-inventory-exceeds-bound");}
            } else {return Err("guest-inventory-invalid");}
        }
        Ok(())
    }
    let mut actual=BTreeSet::new();inventory(root,root,0,&mut actual)?;
    declared.insert("manifest.json".into());
    if actual!=declared {return Err("guest-inventory-mismatch");}
    // Do not accept a manifest replaced while its artifacts were inspected.
    let current=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&manifest).map_err(|_|"guest-manifest-changed")?;
    let mut current_bytes=Vec::new();current.take(2*1024*1024+1).read_to_end(&mut current_bytes).map_err(|_|"guest-manifest-changed")?;
    if current_bytes!=bytes {return Err("guest-manifest-changed");}
    Ok((format!("{:x}",Sha256::digest(&bytes)),entries.len(),total))
}

fn verify_build_config(path:&Path)->Result<(String,BTreeMap<String,String>)> {
    use sha2::{Digest,Sha256};
    use std::io::Read;
    let bytes=private(path)?;
    let value:serde_json::Value=serde_json::from_slice(&bytes)?;
    let fields=value.as_object().context("runtime configuration must be an object")?;
    let required=["qemu","kernel","busybox","firmware","data_dir","library_dir","module_dir","modules","memory_mb"];
    let optional=["toolchain","registry","cargo_git"];
    ensure!(required.iter().all(|key|fields.contains_key(*key)) && fields.keys().all(|key|required.contains(&key.as_str()) || optional.contains(&key.as_str())),"invalid runtime fields");
    ensure!(value["memory_mb"].as_u64().is_some_and(|memory|(512..=4096).contains(&memory)),"invalid runtime memory");
    let modules=value["modules"].as_array().filter(|modules|!modules.is_empty() && modules.len()<=32).context("invalid modules")?;
    let trusted=|path:&Path|->Result<std::fs::Metadata> {
        ensure!(path.is_absolute(),"runtime path must be absolute");
        let metadata=path.metadata()?;
        ensure!((metadata.uid()==0 || metadata.uid()==unsafe{libc::geteuid()}) && metadata.mode() & 0o022==0,"runtime artifact is not trusted");
        Ok(metadata)
    };
    for key in ["data_dir","library_dir","module_dir","toolchain","registry","cargo_git"] {
        if optional.contains(&key) && value.get(key).is_none() {continue;}
        let path=Path::new(value[key].as_str().context("runtime directory missing")?);
        ensure!(path.is_absolute() && path.is_dir(),"invalid runtime directory");
        if !matches!(key,"registry"|"cargo_git") {trusted(path)?;}
    }
    let mut paths=BTreeMap::new();
    for key in ["qemu","kernel","busybox","firmware"] {
        paths.insert(key.to_owned(),PathBuf::from(value[key].as_str().context("runtime artifact missing")?));
    }
    for (index,module) in modules.iter().enumerate() {
        paths.insert(format!("module-{index}"),PathBuf::from(module.as_str().context("invalid module")?));
    }
    let mut hashes=BTreeMap::new();
    for (name,path) in paths {
        let metadata=trusted(&path)?;ensure!(metadata.is_file() && metadata.len()<=512*1024*1024,"invalid runtime file");
        let canonical=path.canonicalize()?;
        let mut file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(canonical)?;
        let before=file.metadata()?;
        ensure!(before.dev()==metadata.dev() && before.ino()==metadata.ino(),"runtime file changed during open");
        let mut digest=Sha256::new();let mut buffer=[0u8;65536];let mut read=0u64;
        loop {let count=file.read(&mut buffer)?;if count==0 {break;}read+=count as u64;
            ensure!(read<=metadata.len(),"runtime file grew while hashing");digest.update(&buffer[..count]);}
        let after=file.metadata()?;
        ensure!(read==before.len() && before.mtime()==after.mtime() && before.mtime_nsec()==after.mtime_nsec()
            && before.ctime()==after.ctime() && before.ctime_nsec()==after.ctime_nsec(),"runtime file changed while hashing");
        hashes.insert(name,format!("{:x}",digest.finalize()));
    }
    Ok((format!("{:x}",Sha256::digest(bytes)),hashes))
}

/// A preflight rejection is authoritative only for the observed declaration.
/// Success is not a role/resource/live readiness claim; launch revalidates it.
pub fn preflight() -> Result<RuntimeReport> {
    let report=inspect();
    ensure!(report.status=="native-guest-verified","native-runtime:{}",report.status);
    ensure!(report.build_status=="build-declaration-verified","native-runtime:{}",report.build_status);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest,Sha256};
    #[test]
    fn build_runtime_requires_complete_trusted_declaration_and_hashes_executables() {
        for scenario in ["valid","missing","public-config","writable-artifact","unknown-field","bad-memory","no-modules"] {
            let root=tempfile::tempdir().unwrap();let config=root.path().join("runtime.json");
            let mut value=serde_json::json!({"memory_mb":1024,"modules":[]});
            for key in ["qemu","kernel","busybox","firmware","module"] {
                let path=root.path().join(key);write(&path,b"trusted fixture").unwrap();
                if key=="module" {value["modules"]=serde_json::json!([path]);} else {value[key]=serde_json::to_value(path).unwrap();}
            }
            for key in ["data_dir","library_dir","module_dir"] {
                let path=root.path().join(key);std::fs::create_dir(&path).unwrap();
                std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o700)).unwrap();value[key]=serde_json::to_value(path).unwrap();
            }
            match scenario {
                "unknown-field"=>value["unexpected"]=true.into(),
                "bad-memory"=>value["memory_mb"]=1.into(),
                "no-modules"=>value["modules"]=serde_json::json!([]),
                "writable-artifact"=>std::fs::set_permissions(root.path().join("qemu"),std::fs::Permissions::from_mode(0o666)).unwrap(),
                _=>(),
            }
            if scenario!="missing" {write(&config,value.to_string().as_bytes()).unwrap();}
            if scenario=="public-config" {std::fs::set_permissions(&config,std::fs::Permissions::from_mode(0o644)).unwrap();}
            let result=verify_build_config(&config);
            assert_eq!(result.is_ok(),scenario=="valid","{scenario}: {result:?}");
            if let Ok((hash,artifacts))=result {
                assert_eq!(hash,format!("{:x}",Sha256::digest(value.to_string().as_bytes())));
                assert_eq!(artifacts.len(),5);
                assert_eq!(artifacts["qemu"],format!("{:x}",Sha256::digest(b"trusted fixture")));
            }
        }
    }
    #[test]
    fn runtime_readiness_verifies_bytes_inventory_and_never_repairs_artifacts() {
        for scenario in ["valid","missing","changed","extra","symlink","traversal","duplicate","public","missing-manifest"] {
            let root=tempfile::tempdir().unwrap();
            let guest=root.path().join("guest");std::fs::create_dir(&guest).unwrap();
            std::fs::set_permissions(&guest,std::fs::Permissions::from_mode(0o700)).unwrap();
            let artifact=guest.join("python");std::fs::write(&artifact,b"runtime fixture").unwrap();
            let entry=serde_json::json!({"path":"python","sha256":format!("{:x}",Sha256::digest(b"runtime fixture"))});
            let mut manifest=serde_json::json!({"version":1,"files":[entry.clone()]});
            match scenario {
                "missing"=>std::fs::remove_file(&artifact).unwrap(),
                "changed"=>std::fs::write(&artifact,b"modified runtime").unwrap(),
                "extra"=>std::fs::write(guest.join("undeclared"),b"extra").unwrap(),
                "symlink"=>{std::fs::remove_file(&artifact).unwrap();std::os::unix::fs::symlink("/etc/hosts",&artifact).unwrap();},
                "traversal"=>manifest["files"][0]["path"]="../escape".into(),
                "duplicate"=>manifest["files"].as_array_mut().unwrap().push(entry),
                "public"=>std::fs::set_permissions(&guest,std::fs::Permissions::from_mode(0o755)).unwrap(),
                _=>(),
            }
            if scenario!="missing-manifest" {std::fs::write(guest.join("manifest.json"),manifest.to_string()).unwrap();}
            let policy=root.path().join("policy.json");
            write(&policy,serde_json::json!({"version":1,"runtime_config":"/runtime.json","guest_runtime":guest,
                "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,"providers":{}}).to_string().as_bytes()).unwrap();
            let before=std::fs::read(&policy).unwrap();
            let report=inspect_at(Some(&policy));
            assert_eq!(report.status=="native-guest-verified",scenario=="valid","{scenario}: {}",report.status);
            assert!(!report.live_verified);
            if scenario=="valid" {assert_eq!(report.verified_files,1);assert_eq!(report.verified_bytes,15);}
            assert_eq!(std::fs::read(&policy).unwrap(),before);
            if scenario=="changed" {assert_eq!(std::fs::read(&artifact).unwrap(),b"modified runtime");}
        }
    }
}
