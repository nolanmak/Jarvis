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
impl Drop for ScopeLock {
    fn drop(&mut self) {
        // Closing our descriptor alone leaves flock held when a concurrent
        // fork inherited it. End the visitor's lock lifetime explicitly.
        unsafe {libc::flock(self.file.as_raw_fd(),libc::LOCK_UN)};
    }
}
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
fn admission_for(worktree:&Path,next:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>)->Result<Admission> {
    let worktree=worktree.canonicalize()?;
    ensure!(!next.is_empty() && allowed.len()<=256 && pinned.len()<=256,"invalid scope admission");
    for path in allowed.iter().map(|p|&p.path).chain(pinned.iter()) {relative(path)?;}
    let admission=Admission {version:1,worktree,contract_hash:next.into(),allowed,pinned};
    ensure!(serde_json::to_vec(&admission)?.len()<=1024*1024,"write admission exceeds bound");Ok(admission)
}
/// Compute a descriptive reference from independently retained permission bytes.
/// Reading a current scope and treating that as the expected source defeats the
/// comparison. This value never grants native execution or publication authority.
pub fn scope_reference_for(worktree:&Path,contract:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>)->Result<ScopeReference> {
    reference_for(&admission_for(worktree,contract,allowed,pinned)?)
}
/// Locked, conditional scope mutation. Acquire under native proof, then enter
/// the workflow transaction and revalidate publication before calling register.
/// Acquiring this object does not change the registered scope. Dropping it never
/// rolls back an effect whose filesystem acknowledgement may have been lost.
pub struct ScopeTransition {admission:Admission,previous:Option<Admission>,path:PathBuf,lock:ScopeLock,registered:std::cell::Cell<bool>}
impl ScopeTransition {
    fn read_current(&self)->Result<Option<Admission>> {
        self.lock.validate_current()?;
        match fs::symlink_metadata(&self.path) {
            Ok(_)=>Ok(Some(read(&self.path)?)),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(e)=>Err(e.into()),
        }
    }
    pub fn register(&self)->Result<RegisteredScope<'_>> {
        let previous=self.read_current()?;
        ensure!(previous==self.previous || previous.as_ref()==Some(&self.admission),"write admission changed before conditional publication");
        let bytes=serde_json::to_vec(&self.admission)?;
        if let Some(previous)=previous.as_ref().filter(|previous|*previous!=&self.admission) {
            let historical=serde_json::to_vec(previous)?;
            let history=self.lock.directory.join(format!("historical-{:x}.json",Sha256::digest(&historical)));
            match fs::symlink_metadata(&history) {
                Ok(_)=>ensure!(read(&history)?==*previous,"historical write admission changed"),
                Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{
                    self.lock.validate_current()?;
                    let mut file=tempfile::NamedTempFile::new_in(&self.lock.directory)?;file.write_all(&historical)?;file.as_file().sync_all()?;
                    file.persist_noclobber(&history).map_err(|e|e.error)?;
                },Err(e)=>return Err(e.into()),
            }
        }
        if previous.as_ref()!=Some(&self.admission) {
            let mut staged=tempfile::NamedTempFile::new_in(&self.lock.directory)?;staged.write_all(&bytes)?;staged.as_file().sync_all()?;
            ensure!(self.read_current()?==previous,"write admission changed during amendment");
            if previous.is_some() {staged.persist(&self.path).map_err(|e|e.error)?;}
            else {staged.persist_noclobber(&self.path).map_err(|e|e.error)?;}
        }
        // Exact replay settles an earlier directory-fsync acknowledgement.
        fs::File::open(&self.lock.directory)?.sync_all()?;
        let proof=RegisteredScope {reference:reference_for(&self.admission)?,path:self.path.clone(),_admission:&self.admission,lock:&self.lock};
        proof.validate_current()?;self.registered.set(true);Ok(proof)
    }
}
enum ExpectedScope<'a> {Contract(Option<&'a str>),Exact(Option<&'a ScopeReference>)}
/// Holds the same lock used by ordinary writers, without writing scope metadata
/// before the visitor's workflow transaction. Exact next permission replay is
/// allowed; any other existing source must match the independently retained
/// reference. Some(next) requires an existing scope, including on replay.
pub fn with_scope_transition<T>(worktree:&Path,expected:Option<&ScopeReference>,next:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>,
    visit:impl FnOnce(&ScopeTransition)->Result<T>)->Result<T> {
    let state=crate::state_dir::state_dir().context("scope control state unavailable")?;
    with_scope_transition_at(&state,worktree,ExpectedScope::Exact(expected),next,allowed,pinned,visit)
}
fn with_scope_transition_at<T>(state:&Path,worktree:&Path,expected:ExpectedScope<'_>,next:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>,
    visit:impl FnOnce(&ScopeTransition)->Result<T>)->Result<T> {
    let admission=admission_for(worktree,next,allowed,pinned)?;
    let lock=ScopeLock::acquire(state,&admission.worktree,true)?;
    let path=lock.directory.join(location(state,&admission.worktree).file_name().context("scope path missing")?);
    let mut transition=ScopeTransition {admission,previous:None,path,lock,registered:std::cell::Cell::new(false)};
    let previous=transition.read_current()?;
    if let Some(previous)=&previous {
        ensure!(previous.version==1 && previous.worktree==transition.admission.worktree,"write admission source identity changed");
        if previous.contract_hash==next {ensure!(previous==&transition.admission,"active write admission differs from registered permissions");}
        if previous!=&transition.admission {
            match expected {
                ExpectedScope::Contract(expected)=>ensure!(expected==Some(previous.contract_hash.as_str()),"write contract changed without a new authorized workflow"),
                ExpectedScope::Exact(expected)=>ensure!(expected==Some(&reference_for(previous)?),"previous registered permissions changed before publication"),
            }
        }
    }else {ensure!(match expected {ExpectedScope::Contract(e)=>e.is_none(),ExpectedScope::Exact(e)=>e.is_none()},"amended write admission source missing");}
    transition.previous=previous;
    let result=visit(&transition)?;transition.lock.validate_current()?;
    if transition.registered.get() {ensure!(transition.read_current()?.as_ref()==Some(&transition.admission),"registered scope changed before transition return");}
    Ok(result)
}
fn with_registered_scope_at<T>(state:&Path,worktree:&Path,expected:Option<&str>,next:&str,allowed:Vec<DeclaredPath>,pinned:Vec<String>,visit:impl FnOnce(&RegisteredScope<'_>)->Result<T>)->Result<T> {
    with_scope_transition_at(state,worktree,ExpectedScope::Contract(expected),next,allowed,pinned,|transition| {
        let proof=transition.register()?;let result=visit(&proof)?;proof.validate_current()?;Ok(result)
    })
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
    fn scope_release_unlocks_even_when_fork_retains_descriptor() {
        const CHILD:&str="AUTOSHIP_SCOPE_RELEASE_FORK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","autoship_write_scope::replay_tests::scope_release_unlocks_even_when_fork_retains_descriptor","--test-threads=1"])
                .env(CHILD,"1").output().unwrap();
            assert!(output.status.success(),"{} {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
        }
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        let mut pipe=[-1;2];assert_eq!(unsafe{libc::pipe(pipe.as_mut_ptr())},0);
        let child=std::cell::Cell::new(0);
        let held=with_registered_scope_at(state.path(),tree.path(),None,"scope",vec![],vec![],|_| {
            let pid=unsafe{libc::fork()};
            if pid==0 {
                // Retain the inherited scope descriptor until parent replay.
                // Only async-signal-safe calls are permitted in this child.
                unsafe {libc::close(pipe[1]);let mut byte=0u8;libc::read(pipe[0],(&mut byte as *mut u8).cast(),1);libc::_exit(0);}
            }
            ensure!(pid>0,"fork failed");child.set(pid);Ok(())
        });
        let replay=held.and_then(|_|register_at(state.path(),tree.path(),"scope",vec![],vec![]));
        // Always release/reap before asserting, including the failing regression.
        unsafe {
            libc::close(pipe[0]);let byte=1u8;libc::write(pipe[1],(&byte as *const u8).cast(),1);libc::close(pipe[1]);
            if child.get()>0 {libc::waitpid(child.get(),std::ptr::null_mut(),0);}
        }
        replay.expect("scope stayed locked after parent release while fork child retained descriptor");
    }
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
    fn publication_scope_rejects_changed_previous_permissions_before_mutation() {
        for fault in ["allowed", "pinned"] {
            let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
            let allowed=vec![DeclaredPath {path:"src".into(),directory:true}];let pinned=vec!["protected.py".to_owned()];
            register_at(state.path(),tree.path(),"old",allowed,pinned).unwrap();
            let expected=current_scope_reference(state.path(),tree.path()).unwrap();
            let path=location(state.path(),&tree.path().canonicalize().unwrap());
            let mut suspect=read(&path).unwrap();
            if fault=="allowed" {suspect.allowed=vec![DeclaredPath {path:"unexpected.rs".into(),directory:false}];}
            else {suspect.pinned=vec!["different.py".into()];}
            let bytes=serde_json::to_vec(&suspect).unwrap();fs::write(&path,&bytes).unwrap();
            let called=std::cell::Cell::new(false);
            let result=with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(Some(&expected)),"next",vec![],vec![],|transition| {called.set(true);transition.register()?;Ok(())});
            assert!(result.is_err(),"{fault}: altered previous permissions accepted under the same contract hash");
            assert!(!called.get());assert_eq!(fs::read(&path).unwrap(),bytes);
        }
    }
    #[test]
    fn publication_scope_transaction_refusal_preserves_previous_scope() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        register_at(state.path(),tree.path(),"old",vec![],vec![]).unwrap();
        let expected=current_scope_reference(state.path(),tree.path()).unwrap();
        let path=location(state.path(),&tree.path().canonicalize().unwrap());let before=fs::read(&path).unwrap();
        let result:Result<()>=with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(Some(&expected)),"next",vec![],vec![],|_| {
            anyhow::bail!("workflow transaction refused publication before scope mutation")
        });
        assert!(result.is_err());assert_eq!(fs::read(&path).unwrap(),before,"scope changed before workflow admission");
    }

    #[test]
    fn publication_scope_effect_survives_transaction_loss_and_replays_exactly() {
        for revision in [false,true] {
            let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
            let expected=if revision {
                register_at(state.path(),tree.path(),"old",vec![],vec![]).unwrap();
                Some(scope_reference_for(tree.path(),"old",vec![],vec![]).unwrap())
            }else {None};
            let allowed=vec![DeclaredPath {path:"src".into(),directory:true}];let pinned=vec!["private.py".into()];
            let next=scope_reference_for(tree.path(),"next",allowed.clone(),pinned.clone()).unwrap();
            let mut db=rusqlite::Connection::open_in_memory().unwrap();db.execute_batch("CREATE TABLE completion (contract TEXT);").unwrap();
            let result:Result<()>=with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(expected.as_ref()),"next",allowed.clone(),pinned.clone(),|transition| {
                let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let proof=transition.register()?;assert_eq!(proof.reference(),&next);
                assert!(register_at(state.path(),tree.path(),"next",allowed.clone(),pinned.clone()).is_err());
                assert!(grants(state.path(),tree.path()).is_err());
                tx.execute("INSERT INTO completion VALUES('next')",[])?;
                anyhow::bail!("completion acknowledgement lost after actual scope effect")
            });
            assert!(result.is_err());assert_eq!(db.query_row("SELECT count(*) FROM completion",[],|r|r.get::<_,i64>(0)).unwrap(),0);
            assert_eq!(current_scope_reference(state.path(),tree.path()).unwrap(),next);
            with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(expected.as_ref()),"next",allowed.clone(),pinned.clone(),|transition| {
                let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let proof=transition.register()?;assert_eq!(proof.reference(),&next);proof.validate_current()?;
                tx.execute("INSERT INTO completion VALUES('next')",[])?;tx.commit()?;Ok(())
            }).unwrap();
            assert_eq!(db.query_row("SELECT count(*) FROM completion",[],|r|r.get::<_,i64>(0)).unwrap(),1);
            // Once canonical publication names next, its caller must require
            // this exact existing effect, rather than recreate a missing scope.
            let path=location(state.path(),&tree.path().canonicalize().unwrap());fs::remove_file(&path).unwrap();
            let called=std::cell::Cell::new(false);
            assert!(with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(Some(&next)),"next",allowed,pinned,|transition| {
                called.set(true);transition.register()?;Ok(())
            }).is_err());assert!(!called.get());assert!(!path.exists());
        }
    }
    #[test]
    fn publication_scope_refuses_mutation_after_registration_even_if_visitor_returns_success() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        let path=location(state.path(),&tree.path().canonicalize().unwrap());
        let result=with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(None),"next",vec![],vec![],|transition| {
            transition.register()?.validate_current()?;
            let mut suspect=read(&path)?;suspect.pinned=vec!["unexpected.py".into()];fs::write(&path,serde_json::to_vec(&suspect)?)?;Ok(())
        });
        assert!(result.is_err());assert_eq!(read(&path).unwrap().pinned,vec!["unexpected.py"]);
    }
    #[test]
    fn publication_scope_rechecks_source_after_entering_workflow_transaction() {
        let state=tempfile::tempdir().unwrap();let tree=tempfile::tempdir().unwrap();
        register_at(state.path(),tree.path(),"old",vec![],vec![]).unwrap();
        let expected=scope_reference_for(tree.path(),"old",vec![],vec![]).unwrap();
        let path=location(state.path(),&tree.path().canonicalize().unwrap());
        let mut suspect=read(&path).unwrap();suspect.pinned=vec!["unexpected.py".into()];let bytes=serde_json::to_vec(&suspect).unwrap();
        let result:Result<()>=with_scope_transition_at(state.path(),tree.path(),ExpectedScope::Exact(Some(&expected)),"next",vec![],vec![],|transition| {
            fs::write(&path,&bytes)?;
            transition.register()?;Ok(())
        });
        assert!(result.is_err());assert_eq!(fs::read(&path).unwrap(),bytes);
    }
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
