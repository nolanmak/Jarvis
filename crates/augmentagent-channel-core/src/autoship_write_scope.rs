//! Controller-authored literal write admission for native autoship workers.
//! Never grants the worktree root. Ancestors of pinned/control files are split
//! into siblings and individual file grants because Landlock cannot subtract.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read,Write},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt,MetadataExt,OpenOptionsExt,PermissionsExt},
    path::{Component, Path, PathBuf},
};

#[path="autoship_scope_materialization.rs"]
mod materialization;
pub(crate) use materialization::ScopeMaterialization;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPath {
    pub path: String,
    pub directory: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    version: u32,
    worktree: PathBuf,
    contract_hash: String,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
}
#[derive(Debug, Default)]
pub(crate) struct Grants {
    pub directories: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub materialization:Option<std::sync::Arc<ScopeMaterialization>>,
}
const CONTROLS: &[&str] = &[".git", ".claude", ".mcp.json", ".github", ".gitmodules"];
fn relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "write scope must be a literal relative path"
    );
    Ok(())
}
fn control(path: &Path) -> bool {
    path.components()
        .any(|c| CONTROLS.iter().any(|name| c.as_os_str() == *name))
}
fn location(state: &Path, worktree: &Path) -> PathBuf {
    state.join("autoship-write-admission").join(format!(
        "{:x}.json",
        Sha256::digest(worktree.as_os_str().as_encoded_bytes())
    ))
}
pub fn register(
    worktree: &Path,
    contract_hash: &str,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
) -> Result<()> {
    let state = crate::state_dir::state_dir().context("scope control state unavailable")?;
    register_at(&state, worktree, contract_hash, allowed, pinned)
}
pub(crate) fn register_at(
    state: &Path,
    worktree: &Path,
    contract_hash: &str,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
) -> Result<()> {
    with_registered_scope_at(state,worktree,None,contract_hash,allowed,pinned,|_|Ok(()))
}
fn read(path: &Path) -> Result<Admission> {
    let mut file=fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    let m=file.metadata()?;
    ensure!(m.is_file() && m.nlink()==1 && m.uid()==unsafe{libc::geteuid()} && m.mode()&0o077==0 && m.len()<=1024*1024,"write admission is not private");
    let mut bytes=Vec::new();(&mut file).take(1024*1024+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=1024*1024 && bytes.len() as u64==m.len(),"write admission changed while reading");
    let admission:Admission=serde_json::from_slice(&bytes)?;
    ensure!(serde_json::to_vec(&admission)?==bytes,"write admission encoding changed");
    Ok(admission)
}
struct ScopeLock {file:fs::File,path:PathBuf,directory:PathBuf,directory_device:u64,directory_inode:u64}
impl ScopeLock {
    fn acquire(state:&Path,worktree:&Path,create:bool)->Result<Self> {
        if create {fs::create_dir_all(state)?;}
        let state=state.canonicalize()?;let directory=state.join("autoship-write-admission");
        ensure!(!directory.starts_with(worktree),"write admission cannot live in candidate");
        if create {match fs::DirBuilder::new().mode(0o700).create(&directory) {Ok(())=>(),Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>(),Err(e)=>return Err(e.into())}}
        let parent=fs::symlink_metadata(&directory)?;
        ensure!(parent.is_dir() && !parent.file_type().is_symlink() && parent.uid()==unsafe{libc::geteuid()},"write scope directory is not owned/protected");
        // Older writers used create_dir_all with the process umask. Tighten
        // only this verified owned directory, via its no-follow descriptor.
        let directory_file=fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&directory)?;
        let opened=directory_file.metadata()?;
        ensure!(opened.dev()==parent.dev() && opened.ino()==parent.ino(),"write scope directory changed during admission");
        if opened.mode()&0o022!=0 {directory_file.set_permissions(fs::Permissions::from_mode(0o700))?;}

        let path=location(&state,worktree).with_extension("lock");
        let file=fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path)?;
        let m=file.metadata()?;
        ensure!(m.is_file() && m.nlink()==1 && m.uid()==unsafe{libc::geteuid()} && m.mode()&0o077==0,"write scope lock is not private");
        ensure!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}==0,"write scope busy; retry with fresh metadata admission");
        let lock=Self {file,path,directory,directory_device:parent.dev(),directory_inode:parent.ino()};lock.validate_current()?;Ok(lock)
    }
    fn validate_current(&self)->Result<()> {
        let parent=fs::symlink_metadata(&self.directory)?;let actual=fs::symlink_metadata(&self.path)?;let held=self.file.metadata()?;
        ensure!(parent.is_dir() && !parent.file_type().is_symlink() && parent.dev()==self.directory_device && parent.ino()==self.directory_inode
            && parent.uid()==unsafe{libc::geteuid()} && parent.mode()&0o022==0
            && actual.is_file() && !actual.file_type().is_symlink() && actual.dev()==held.dev() && actual.ino()==held.ino()
            && actual.nlink()==1 && actual.uid()==unsafe{libc::geteuid()} && actual.mode()&0o077==0,"write scope lock identity changed");Ok(())
    }
}
fn ensure_parent(root: &Path, relative_path: &Path) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative_path.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(m) => ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "write parent is not a real directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}
pub(crate) fn grants(state: &Path, worktree: &Path) -> Result<Grants> {
    let worktree = worktree.canonicalize()?;
    let _scope=ScopeLock::acquire(state,&worktree,false)?;
    let admission = read(&location(state, &worktree))
        .context("native worker has no registered literal write scope")?;
    ensure!(
        admission.version == 1 && admission.worktree == worktree,
        "write admission identity mismatch"
    );
    let mut materialization=ScopeMaterialization::new(reference_for(&admission)?);
    let pins: Vec<PathBuf> = admission.pinned.iter().map(|p| worktree.join(p)).collect();
    let mut grants = Grants::default();
    for declared in admission.allowed {
        relative(&declared.path)?;
        let relative_path = Path::new(&declared.path);
        if control(relative_path) {
            continue;
        }
        let path = worktree.join(relative_path);
        if pins.iter().any(|pin| path == *pin || path.starts_with(pin)) {
            continue;
        }
        if declared.directory {
            ensure_parent(&worktree, relative_path)?;
            split(&worktree, &path, &pins, &mut grants)?;
        } else {
            ensure_parent(&worktree, relative_path.parent().unwrap_or(Path::new("")))?;
            if !path.try_exists()? {
                materialization.create(&declared.path)?;
            }
            file_grant(&path, &mut grants)?;
        }
    }
    grants.directories.sort();
    grants.directories.dedup();
    grants.files.sort();
    grants.files.dedup();
    ensure!(
        grants.files.len() + grants.directories.len() <= 10000,
        "write admission expands beyond limit"
    );
    _scope.validate_current()?;
    if materialization.paths().next().is_some() {grants.materialization=Some(std::sync::Arc::new(materialization));}
    Ok(grants)
}
fn file_grant(path: &Path, grants: &mut Grants) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.nlink() == 1,
        "write file is not singly linked regular file"
    );
    grants.files.push(path.to_path_buf());
    Ok(())
}
fn split(root: &Path, path: &Path, pins: &[PathBuf], grants: &mut Grants) -> Result<()> {
    let mut protected = pins.iter().any(|pin| pin.starts_with(path));
    let mut entries = Vec::new();
    // Detect existing controls/symlinks anywhere beneath a prospective broad
    // grant. If any exist, recurse without granting their ancestor directory.
    fn has_control(root: &Path, path: &Path, count: &mut usize) -> Result<bool> {
        *count += 1;
        ensure!(*count <= 100000, "write scope scan exceeds limit");
        if control(path.strip_prefix(root)?) {
            return Ok(true);
        }
        let m = fs::symlink_metadata(path)?;
        if m.file_type().is_symlink() || (m.is_file() && m.nlink() != 1) {
            return Ok(true);
        }
        if m.is_dir() {
            for entry in fs::read_dir(path)? {
                if has_control(root, &entry?.path(), count)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    protected |= has_control(root, path, &mut 0)?;
    if !protected {
        grants.directories.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        entries.push(entry?.path())
    }
    for entry in entries {
        if control(entry.strip_prefix(root)?) || pins.iter().any(|p| entry == *p) {
            continue;
        }
        let m = fs::symlink_metadata(&entry)?;
        if m.file_type().is_symlink() || (m.is_file() && m.nlink() != 1) {
            continue;
        }
        if m.is_dir() {
            split(root, &entry, pins, grants)?
        } else if m.is_file() {
            file_grant(&entry, grants)?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_ancestors_are_split_and_new_literal_files_precreated() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src/tests")).unwrap();
        fs::create_dir(root.path().join("src/impl")).unwrap();
        fs::write(root.path().join("src/tests/pinned.rs"), "trusted").unwrap();
        fs::write(root.path().join("src/tests/edit.rs"), "code").unwrap();
        register_at(
            state.path(),
            root.path(),
            "hash",
            vec![
                DeclaredPath {
                    path: "src".into(),
                    directory: true,
                },
                DeclaredPath {
                    path: "new.rs".into(),
                    directory: false,
                },
            ],
            vec!["src/tests/pinned.rs".into()],
        )
        .unwrap();
        let result = grants(state.path(), root.path()).unwrap();
        assert!(!result.directories.contains(&root.path().join("src")));
        assert!(!result.directories.contains(&root.path().join("src/tests")));
        assert!(result.directories.contains(&root.path().join("src/impl")));
        assert!(result
            .files
            .contains(&root.path().join("src/tests/edit.rs")));
        assert!(root.path().join("new.rs").is_file());
        assert!(!result
            .files
            .contains(&root.path().join("src/tests/pinned.rs")));
    }
}

/// Descriptive exact scope reference, never native execution authority.
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeReference {pub version:u32,pub worktree:PathBuf,pub contract_hash:String,pub record_sha256:String}
/// Borrowed locked scope metadata. It is not native/source/dispatch authority.
/// The callback must acquire workflow metadata only after this scope lock.
pub struct RegisteredScope<'a> {reference:ScopeReference,path:PathBuf,_admission:&'a Admission,lock:&'a ScopeLock}
impl RegisteredScope<'_> {
    pub fn reference(&self)->&ScopeReference {&self.reference}
    pub fn validate_current(&self)->Result<()> {
        self.lock.validate_current()?;
        let current=read(&self.path)?;
        ensure!(current==*self._admission && format!("{:x}",Sha256::digest(serde_json::to_vec(&current)?))==self.reference.record_sha256,"registered scope changed during metadata visitor");Ok(())
    }
}
fn reference_for(admission:&Admission)->Result<ScopeReference> {
    Ok(ScopeReference {version:1,worktree:admission.worktree.clone(),contract_hash:admission.contract_hash.clone(),
        record_sha256:format!("{:x}",Sha256::digest(serde_json::to_vec(admission)?))})
}
pub(crate) fn current_scope_reference(state:&Path,worktree:&Path)->Result<ScopeReference> {
    let worktree=worktree.canonicalize()?;let lock=ScopeLock::acquire(state,&worktree,false)?;
    let admission=read(&location(state,&worktree))?;
    ensure!(admission.version==1 && admission.worktree==worktree,"current scope identity changed");lock.validate_current()?;reference_for(&admission)
}
pub fn with_registered_scope<T>(worktree:&Path,expected:Option<&str>,next:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>,visit:impl FnOnce(&RegisteredScope<'_>)->Result<T>)->Result<T> {
    let state=crate::state_dir::state_dir().context("scope control state unavailable")?;
    with_registered_scope_at(&state,worktree,expected,next,allowed,pinned,visit)
}
fn with_registered_scope_at<T>(state:&Path,worktree:&Path,expected:Option<&str>,next:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>,visit:impl FnOnce(&RegisteredScope<'_>)->Result<T>)->Result<T> {
    let worktree=worktree.canonicalize()?;
    ensure!(!next.is_empty() && allowed.len()<=256 && pinned.len()<=256,"invalid scope admission");
    for path in allowed.iter().map(|p|&p.path).chain(pinned.iter()) {relative(path)?;}
    let lock=ScopeLock::acquire(state,&worktree,true)?;
    let path=lock.directory.join(location(state,&worktree).file_name().context("scope path missing")?);
    let admission=Admission {version:1,worktree:worktree.clone(),contract_hash:next.into(),allowed,pinned};
    let previous=match fs::symlink_metadata(&path) {
        Ok(_)=>Some(read(&path)?),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>None,Err(e)=>return Err(e.into()),
    };
    let bytes=serde_json::to_vec(&admission)?;ensure!(bytes.len()<=1024*1024,"write admission exceeds bound");
    if let Some(previous)=&previous {
        ensure!(previous.version==1 && previous.worktree==worktree,"write admission source identity changed");
        if previous.contract_hash==next {ensure!(previous==&admission,"active write admission differs from registered permissions");}
        if previous!=&admission {
            ensure!(expected==Some(previous.contract_hash.as_str()),"write contract changed without a new authorized workflow");
            let historical=serde_json::to_vec(previous)?;
            let history=path.parent().unwrap().join(format!("historical-{:x}.json",Sha256::digest(&historical)));
            match fs::symlink_metadata(&history) {
                Ok(_)=>ensure!(read(&history)?==*previous,"historical write admission changed"),
                Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{
                    lock.validate_current()?;
                    let mut file=tempfile::NamedTempFile::new_in(&lock.directory)?;file.write_all(&historical)?;file.as_file().sync_all()?;
                    file.persist_noclobber(&history).map_err(|e|e.error)?;
                },Err(e)=>return Err(e.into()),
            }
        }
    }else {ensure!(expected.is_none(),"amended write admission source missing");}
    if previous.as_ref()!=Some(&admission) {
        let mut staged=tempfile::NamedTempFile::new_in(&lock.directory)?;staged.write_all(&bytes)?;staged.as_file().sync_all()?;
        lock.validate_current()?;
        if let Some(previous)=&previous {
            ensure!(read(&path)?==*previous,"write admission changed during amendment");staged.persist(&path).map_err(|e|e.error)?;
        }else {staged.persist_noclobber(&path).map_err(|e|e.error)?;}
    }
    // Idempotent replay also settles an earlier directory-fsync acknowledgement.
    fs::File::open(&lock.directory)?.sync_all()?;
    let reference=ScopeReference {version:1,worktree,contract_hash:next.into(),record_sha256:format!("{:x}",Sha256::digest(&bytes))};
    let proof=RegisteredScope {reference,path,_admission:&admission,lock:&lock};proof.validate_current()?;
    let result=visit(&proof)?;proof.validate_current()?;Ok(result)
}

/// Controller-only activation after independently verified contract amendment.
/// Candidate tools cannot write this private store or invoke this Rust API.
pub fn replace_registered(
    worktree: &Path,
    expected_contract: &str,
    next_contract: &str,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
) -> Result<()> {
    let state = crate::state_dir::state_dir().context("scope control state unavailable")?;
    replace_registered_at(&state,worktree,expected_contract,next_contract,allowed,pinned)
}
fn replace_registered_at(state:&Path,worktree:&Path,expected_contract:&str,next_contract:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>)->Result<()> {
    with_registered_scope_at(state,worktree,Some(expected_contract),next_contract,allowed,pinned,|_|Ok(()))
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    #[test]
    fn registered_scope_replacement_replays_only_the_exact_permissions() {
        let worktree=tempfile::tempdir().unwrap();
        register(worktree.path(),"previous",vec![DeclaredPath {path:"old.rs".into(),directory:false}],vec![]).unwrap();
        let allowed=vec![DeclaredPath {path:"src".into(),directory:true}];let pinned=vec!["private/observe.py".to_owned()];
        replace_registered(worktree.path(),"previous","next",allowed.clone(),pinned.clone()).unwrap();
        replace_registered(worktree.path(),"previous","next",allowed.clone(),pinned.clone()).unwrap();
        assert!(replace_registered(worktree.path(),"stale","other",allowed.clone(),pinned.clone()).is_err());
        register(worktree.path(),"next",allowed,pinned).unwrap();
    }
    #[test]
    fn active_contract_replay_refuses_different_allowed_or_pinned_permissions() {
        for fault in ["allowed", "pinned"] {
            let worktree=tempfile::tempdir().unwrap();
            let allowed=vec![DeclaredPath {path:"src".into(),directory:true}];
            let pinned=vec!["private/observe.py".to_owned()];
            let stored_allowed=if fault=="allowed" {vec![DeclaredPath {path:"owner.txt".into(),directory:false}]}else{allowed.clone()};
            let stored_pinned=if fault=="pinned" {vec!["private/different.py".to_owned()]}else{pinned.clone()};
            register(worktree.path(),"next",stored_allowed.clone(),stored_pinned.clone()).unwrap();
            assert!(replace_registered(worktree.path(),"previous","next",allowed,pinned).is_err(),"{fault}: same contract name accepted different permission bytes");
            // Refusal preserves the suspect record for inspection instead of
            // silently rewriting it or granting the requested replacement.
            register(worktree.path(),"next",stored_allowed,stored_pinned).unwrap();
        }
    }
}

#[cfg(test)]
mod publication_lock_tests {
    use super::*;
    #[test]
    fn registered_scope_visitor_excludes_other_registered_writers_until_return() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        with_registered_scope_at(state.path(),tree.path(),None,"first",vec![],vec![],|proof| {
            assert_eq!(proof.reference().contract_hash,"first");
            assert!(replace_registered_at(state.path(),tree.path(),"first","second",vec![],vec![]).is_err(),"concurrent scope replacement crossed retained publication boundary");
            assert!(register_at(state.path(),tree.path(),"first",vec![],vec![]).is_err(),"ordinary scope registration did not join publication lock");
            assert!(grants(state.path(),tree.path()).is_err(),"native grants read passed an unfinished publication boundary");
            proof.validate_current()
        }).unwrap();
        replace_registered_at(state.path(),tree.path(),"first","second",vec![],vec![]).unwrap();
        assert_eq!(read(&location(state.path(),&tree.path().canonicalize().unwrap())).unwrap().contract_hash,"second");
    }
}

#[cfg(test)]
mod scope_integrity_tests {
    use super::*;
    #[test]
    fn scope_same_contract_cannot_rewrite_permissions_and_failed_visitor_keeps_effect() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        register_at(state.path(),tree.path(),"first",vec![],vec![]).unwrap();
        assert!(replace_registered_at(state.path(),tree.path(),"first","first",vec![DeclaredPath {path:"extra.rs".into(),directory:false}],vec![]).is_err());
        let result:Result<()>=with_registered_scope_at(state.path(),tree.path(),Some("first"),"second",vec![],vec![],|proof| {
            proof.validate_current()?;anyhow::bail!("injected canonical publication failure")
        });
        assert!(result.is_err());
        assert_eq!(read(&location(state.path(),&tree.path().canonicalize().unwrap())).unwrap().contract_hash,"second");
        with_registered_scope_at(state.path(),tree.path(),Some("first"),"second",vec![],vec![],|proof|proof.validate_current()).unwrap();
    }
    #[test]
    fn scope_proof_rechecks_replaced_lock_and_retains_suspect_metadata() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        let result:Result<()>=with_registered_scope_at(state.path(),tree.path(),None,"first",vec![],vec![],|proof| {
            let path=proof.path.with_extension("lock");fs::remove_file(&path)?;
            fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&path)?;
            proof.validate_current()
        });
        assert!(result.is_err(),"replaced lock inode retained scope proof");
        assert_eq!(read(&location(state.path(),&tree.path().canonicalize().unwrap())).unwrap().contract_hash,"first");
    }
    #[test]
    fn scope_registration_tightens_legacy_owned_parent_and_checks_history_bytes() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        let parent=state.path().join("autoship-write-admission");fs::create_dir(&parent).unwrap();fs::set_permissions(&parent,fs::Permissions::from_mode(0o777)).unwrap();
        register_at(state.path(),tree.path(),"first",vec![],vec![]).unwrap();
        assert_eq!(fs::metadata(&parent).unwrap().mode()&0o077,0);
        let path=location(state.path(),&tree.path().canonicalize().unwrap());let previous=fs::read(&path).unwrap();
        replace_registered_at(state.path(),tree.path(),"first","second",vec![],vec![]).unwrap();
        replace_registered_at(state.path(),tree.path(),"second","first",vec![],vec![]).unwrap();
        let history=parent.join(format!("historical-{:x}.json",Sha256::digest(&previous)));fs::write(history,b"{}").unwrap();
        assert!(replace_registered_at(state.path(),tree.path(),"first","third",vec![],vec![]).is_err());
        assert_eq!(fs::read(&path).unwrap(),previous,"damaged historical scope was silently replaced");
    }
    #[test]
    fn scope_fifo_refuses_without_blocking_metadata_admission() {
        const CHILD:&str="JARVIS_SCOPE_FIFO_CHILD";
        if let Some(root)=std::env::var_os(CHILD) {
            let root=PathBuf::from(root);let state=root.join("state");let tree=root.join("tree");fs::create_dir(&state).unwrap();fs::create_dir(&tree).unwrap();
            let path=location(&state,&tree.canonicalize().unwrap());fs::create_dir(path.parent().unwrap()).unwrap();
            let raw=std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(raw.as_ptr(),0o600)},0);
            assert!(register_at(&state,&tree,"first",vec![],vec![]).is_err());return;
        }
        let root=tempfile::tempdir_in("/tmp").unwrap();let mut child=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","autoship_write_scope::scope_integrity_tests::scope_fifo_refuses_without_blocking_metadata_admission"]).env(CHILD,root.path()).spawn().unwrap();
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(3);
        loop {
            if let Some(status)=child.try_wait().unwrap() {assert!(status.success());break;}
            if std::time::Instant::now()>=deadline {child.kill().unwrap();child.wait().unwrap();panic!("scope FIFO blocked metadata admission");}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
