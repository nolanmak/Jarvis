//! Controller filesystem readiness. Never publishes an acceptance artifact.
use super::*;
use std::os::unix::fs::{MetadataExt,OpenOptionsExt};
const PROBE_MARKER:&[u8]=b"jarvis-publication-readiness-v1\n";
const PROBE_STAGE:&str=".jarvis-recovery-publication-stage";
const PROBE_FINAL:&str=".jarvis-recovery-publication-final";
const MAX_PROBE_BYTES:usize=4*1024*1024+3*64*1024+PROBE_MARKER.len();


pub(super) fn probe_registration(publication:&augmentagent_channel_core::autoship_write_scope::RegistrationPublication)->Result<()> {
    ensure!(publication.bytes.len()<=1024*1024,"registration publication exceeds envelope");
    if publication.registered {return Ok(());}
    let parent=prepare_registration_parent(publication.path.parent().context("registration parent missing")?)?;
    match std::fs::symlink_metadata(&publication.path) {
        Ok(metadata)=>{
            ensure!(metadata.is_file() && !metadata.file_type().is_symlink(),"registration destination is not a regular file");
            ensure!(baseline_witness::private_bytes(&publication.path,1024*1024)?==publication.bytes,"registration publication conflicts");
        },
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
        Err(error)=>return Err(error.into()),
    }
    named_probe(&parent,publication.bytes.len()+64*1024,write_allocation)
}

fn prepare_registration_parent(path:&Path)->Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    ensure!(path.is_absolute() && path.components().all(|part|matches!(part,std::path::Component::RootDir|std::path::Component::Normal(_))),
        "registration control path is not absolute and literal");
    let mut current=path.to_path_buf();let mut missing=Vec::new();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(meta)=>{
                ensure!(meta.is_dir() && !meta.file_type().is_symlink() && current.canonicalize()?==current,
                    "registration parent is redirected");
                ensure!(meta.uid()==unsafe{libc::geteuid()} && meta.mode()&0o022==0 && meta.mode()&0o300==0o300,
                    "registration storage is not owned writable control state");
                break;
            },
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
                missing.push(current.file_name().context("registration ancestor missing")?.to_os_string());
                ensure!(current.pop(),"registration root missing");
            },
            Err(error)=>return Err(error.into()),
        }
    }
    for part in missing.into_iter().rev() {
        current.push(part);
        match std::fs::DirBuilder::new().mode(0o700).create(&current) {
            Ok(())=>{},Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{},Err(error)=>return Err(error.into()),
        }
        let meta=std::fs::symlink_metadata(&current)?;
        ensure!(meta.is_dir() && !meta.file_type().is_symlink() && meta.uid()==unsafe{libc::geteuid()}
            && meta.mode()&0o022==0 && meta.mode()&0o300==0o300,"registration control directory changed");
    }
    Ok(current)
}

fn write_allocation(file:&mut std::fs::File,length:usize)->Result<()> {
    use std::io::{Read,Write};
    let mut entropy=std::fs::File::open("/dev/urandom")?;
    let mut block=[0u8;65536];let mut remaining=length;
    while remaining>0 {let count=remaining.min(block.len());entropy.read_exact(&mut block[..count])?;file.write_all(&block[..count])?;remaining-=count;}
    file.sync_all()?;Ok(())
}

pub(super) fn probe(control:&Path,receipt:&Path,expected:&[u8],additional_bytes:usize)->Result<()> {
    probe_with(control,receipt,expected,additional_bytes,write_allocation)
}
fn probe_with(control:&Path,receipt:&Path,expected:&[u8],additional_bytes:usize,
    write:impl FnOnce(&mut std::fs::File,usize)->Result<()>)->Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(expected.len()<=64*1024 && additional_bytes<=4*1024*1024+64*1024,"publication readiness exceeds envelope");
    let metadata=std::fs::symlink_metadata(control)?;
    ensure!(metadata.is_dir() && metadata.uid()==unsafe{libc::geteuid()} && metadata.mode()&0o077==0
        && metadata.mode()&0o300==0o300,"publication directory is not private writable controller storage");
    let control=control.canonicalize()?;
    ensure!(receipt.parent().context("publication receipt parent missing")?.canonicalize()?==control,
        "publication receipt leaves controller storage");
    match std::fs::symlink_metadata(receipt) {
        Ok(_)=>ensure!(baseline_witness::private_bytes(receipt,64*1024)?==expected,"immutable qualification receipt conflicts"),
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
        Err(error)=>return Err(error.into()),
    }
    named_probe(&control,expected.len()+additional_bytes+64*1024,write)
}

struct ProbeLock(std::fs::File);
impl std::os::fd::AsRawFd for ProbeLock {
    fn as_raw_fd(&self)->std::os::fd::RawFd {std::os::fd::AsRawFd::as_raw_fd(&self.0)}
}
impl Drop for ProbeLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Explicit unlock also releases a transient pre-exec fork's inherited
        // open description; it cannot extend readiness ownership after return.
        unsafe{libc::flock(self.0.as_raw_fd(),libc::LOCK_UN)};
    }
}
fn probe_lock(control:&Path)->Result<ProbeLock> {
    use std::os::fd::AsRawFd;
    let file=std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(control)?;
    let meta=file.metadata()?;
    ensure!(meta.is_dir() && meta.uid()==unsafe{libc::geteuid()} && meta.mode()&0o022==0,
        "publication probe lock is not owned control storage");
    if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {return Err(std::io::Error::last_os_error().into());}
    Ok(ProbeLock(file))
}

/// Fixed reserved names bound interruption storage; the live descriptor lock
/// excludes another probe. Never sweep unrelated files or receipt names.
fn reclaim_probe(control:&Path)->Result<()> {
    use std::io::Read;
    let mut owned=Vec::new();
    for name in [PROBE_STAGE,PROBE_FINAL] {
        let path=control.join(name);
        let file=match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path) {
            Ok(file)=>file,
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
                ensure!(std::fs::symlink_metadata(&path).is_err_and(|error|error.kind()==std::io::ErrorKind::NotFound),"redirected publication probe state");
                continue;
            },
            Err(error)=>return Err(error.into()),
        };
        let meta=file.metadata()?;
        ensure!(meta.is_file() && meta.uid()==unsafe{libc::geteuid()} && meta.mode()&0o077==0
            && (1..=2).contains(&meta.nlink()) && meta.len()<=MAX_PROBE_BYTES as u64,"publication probe scratch is not bounded owned state");
        let mut prefix=Vec::new();file.take(PROBE_MARKER.len() as u64).read_to_end(&mut prefix)?;
        ensure!(PROBE_MARKER.starts_with(&prefix),"publication probe scratch marker changed");
        owned.push((path,meta));
    }
    for (_,meta) in &owned {
        if meta.nlink()==2 {
            ensure!(owned.len()==2 && owned.iter().all(|(_,other)|other.dev()==meta.dev() && other.ino()==meta.ino()),
                "publication probe has an unowned hard link");
        }
    }
    for (path,meta) in owned {
        let current=std::fs::symlink_metadata(&path)?;
        ensure!(current.dev()==meta.dev() && current.ino()==meta.ino() && current.is_file(),"publication probe scratch was replaced");
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn named_probe(control:&Path,length:usize,write:impl FnOnce(&mut std::fs::File,usize)->Result<()>)->Result<()> {
    use std::io::Write;
    ensure!(length+PROBE_MARKER.len()<=MAX_PROBE_BYTES,"publication probe allocation exceeds envelope");
    let _lock=probe_lock(control)?;
    reclaim_probe(control)?;
    let stage=control.join(PROBE_STAGE);let final_path=control.join(PROBE_FINAL);
    let file=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&stage)?;
    let mut staged=tempfile::NamedTempFile::from_parts(file,tempfile::TempPath::try_from_path(stage)?);
    staged.write_all(PROBE_MARKER)?;
    write(staged.as_file_mut(),length)?;
    staged.as_file().sync_all()?;
    // Use the actual immutable publisher's no-clobber operation, not a mode
    // or anonymous-allocation approximation of it.
    let published=staged.persist_noclobber(&final_path).map_err(|error|error.error)?;
    std::fs::File::open(control)?.sync_all()?;
    drop(published);reclaim_probe(control)?;
    std::fs::File::open(control)?.sync_all()?;Ok(())
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn registration_probe_checks_its_actual_destination_without_creating_a_grant() {
        let root=tempfile::tempdir().unwrap();let parent=root.path().join("admission");
        std::fs::create_dir(&parent).unwrap();std::fs::set_permissions(&parent,std::fs::Permissions::from_mode(0o500)).unwrap();
        let publication=augmentagent_channel_core::autoship_write_scope::RegistrationPublication {path:parent.join("scope.json"),bytes:b"original serialized admission".to_vec(),registered:false};
        assert!(probe_registration(&publication).is_err(),"unwritable admission destination passed recovery readiness");
        std::fs::set_permissions(&parent,std::fs::Permissions::from_mode(0o755)).unwrap();
        probe_registration(&publication).unwrap();
        assert!(!publication.path.exists(),"readiness created a native write grant");
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(),0,"registration readiness leaked scratch");
        let nested=augmentagent_channel_core::autoship_write_scope::RegistrationPublication {path:parent.join("new/child/scope.json"),bytes:b"bounded admission".to_vec(),registered:false};
        probe_registration(&nested).unwrap();assert!(nested.path.parent().unwrap().is_dir());assert!(!nested.path.exists());
        let redirect=root.path().join("redirect");std::os::unix::fs::symlink(&parent,&redirect).unwrap();
        let redirected=augmentagent_channel_core::autoship_write_scope::RegistrationPublication {path:redirect.join("scope.json"),bytes:vec![],registered:false};
        assert!(probe_registration(&redirected).is_err(),"registration readiness followed a redirected control parent");
    }

    #[test]
    fn publication_probe_reclaims_only_owned_interrupted_publication() {
        let root=tempfile::tempdir().unwrap();let control=root.path();
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o700)).unwrap();
        let stage=control.join(".jarvis-recovery-publication-stage");
        std::fs::write(&stage,b"jarvis-publication-readiness-v1\n\0\0").unwrap();
        std::fs::set_permissions(&stage,std::fs::Permissions::from_mode(0o600)).unwrap();
        probe(control,&control.join("receipt.json"),b"expected",128).unwrap();
        assert!(!stage.exists(),"interrupted readiness publication was never reconciled");
        assert!(!control.join(".jarvis-recovery-publication-final").exists());
    }

    #[test]
    fn publication_probe_reconciles_process_death_and_respects_live_lock() {
        use std::os::fd::AsRawFd;
        const CHILD:&str="JARVIS_NAMED_PROBE_CRASH_FIXTURE";
        if let Some(path)=std::env::var_os(CHILD).map(PathBuf::from) {
            let _lock=probe_lock(&path).unwrap();
            let stage=path.join(PROBE_STAGE);std::fs::write(&stage,PROBE_MARKER).unwrap();
            std::fs::set_permissions(&stage,std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::hard_link(stage,path.join(PROBE_FINAL)).unwrap();
            std::process::exit(0); // no destructor: interrupted no-clobber fallback
        }
        let root=tempfile::tempdir().unwrap();let control=root.path();
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o700)).unwrap();
        let out=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::acceptance::recovery_publication::tests::publication_probe_reconciles_process_death_and_respects_live_lock","--nocapture"])
            .env(CHILD,control).output().unwrap();assert!(out.status.success());
        let lock=probe_lock(control).unwrap();
        assert!(probe(control,&control.join("receipt.json"),b"expected",128).is_err(),"active probe lock was ignored");
        assert!(control.join(PROBE_STAGE).exists(),"active scratch was pruned");
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);drop(lock);
        probe(control,&control.join("receipt.json"),b"expected",128).unwrap();
        assert!(!control.join(PROBE_STAGE).exists());assert!(!control.join(PROBE_FINAL).exists());
        std::fs::write(control.join(PROBE_STAGE),b"unrelated private bytes").unwrap();
        std::fs::set_permissions(control.join(PROBE_STAGE),std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(probe(control,&control.join("receipt.json"),b"expected",128).is_err());
        assert_eq!(std::fs::read(control.join(PROBE_STAGE)).unwrap(),b"unrelated private bytes");
    }

    #[test]
    fn publication_probe_refuses_nonregular_or_externally_linked_scratch() {
        use std::os::unix::ffi::OsStrExt;
        let root=tempfile::tempdir().unwrap();let control=root.path();
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o700)).unwrap();
        let stage=control.join(PROBE_STAGE);let raw=std::ffi::CString::new(stage.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe{libc::mkfifo(raw.as_ptr(),0o600)},0);
        assert!(probe(control,&control.join("receipt.json"),b"expected",128).is_err());
        assert!(std::fs::symlink_metadata(&stage).is_ok());std::fs::remove_file(&stage).unwrap();
        std::fs::write(&stage,PROBE_MARKER).unwrap();std::fs::set_permissions(&stage,std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&stage,control.join("unowned-link")).unwrap();
        assert!(probe(control,&control.join("receipt.json"),b"expected",128).is_err());
        assert!(stage.exists() && control.join("unowned-link").exists());
    }

    #[test]
    fn publication_probe_exercises_no_clobber_and_preserves_conflicting_state() {
        use std::io::Write;
        let root=tempfile::tempdir().unwrap();let control=root.path();
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o700)).unwrap();
        let receipt=control.join("receipt.json");std::fs::write(&receipt,b"expected").unwrap();
        std::fs::set_permissions(&receipt,std::fs::Permissions::from_mode(0o600)).unwrap();
        let result=probe_with(control,&receipt,b"expected",128,|file,_| {
            file.write_all(b"bounded readiness scratch")?;
            // Simulate a competing metadata change after allocation succeeds.
            std::fs::write(control.join(PROBE_FINAL),b"unrecognized state")?;
            std::fs::set_permissions(control.join(PROBE_FINAL),std::fs::Permissions::from_mode(0o600))?;
            Ok(())
        });
        assert!(result.is_err(),"allocation success masked actual no-clobber failure");
        assert_eq!(std::fs::read(&receipt).unwrap(),b"expected");
        assert_eq!(std::fs::read(control.join(PROBE_FINAL)).unwrap(),b"unrecognized state");
        assert!(!control.join(PROBE_STAGE).exists(),"ordinary failed publication leaked staging");
        assert!(probe(control,&receipt,b"expected",128).is_err(),"unknown failed state became a successful retry");
    }

    #[test]
    fn publication_probe_preserves_receipts_and_cleans_up_write_failure() {
        let root=tempfile::tempdir().unwrap();let receipt=root.path().join("receipt.json");
        std::fs::set_permissions(root.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&receipt,b"expected").unwrap();
        std::fs::set_permissions(&receipt,std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(probe_with(root.path(),&receipt,b"expected",128,|_,_|Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into())).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(),1,"only the unchanged receipt may remain");
        assert_eq!(std::fs::read(&receipt).unwrap(),b"expected");
        probe(root.path(),&receipt,b"expected",128).unwrap();
        std::fs::set_permissions(&receipt,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(probe(root.path(),&receipt,b"expected",128).is_err());
        std::fs::remove_file(&receipt).unwrap();
        std::os::unix::fs::symlink(root.path().join("absent"),&receipt).unwrap();
        assert!(probe(root.path(),&receipt,b"expected",128).is_err());
        assert!(probe(root.path(),&receipt,b"expected",5*1024*1024).is_err());
    }
    #[test]
    fn publication_probe_rejects_conflicting_receipts_and_unwritable_storage() {
        let root=tempfile::tempdir().unwrap();let control=root.path();let receipt=control.join("receipt.json");
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&receipt,b"conflict").unwrap();
        std::fs::set_permissions(&receipt,std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(probe(control,&receipt,b"expected",128).is_err(),"conflicting immutable receipt was considered ready");
        assert_eq!(std::fs::read(&receipt).unwrap(),b"conflict");
        std::fs::remove_file(&receipt).unwrap();
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(probe(control,&receipt,b"expected",128).is_err(),"unwritable publication directory was considered ready");
        std::fs::set_permissions(control,std::fs::Permissions::from_mode(0o700)).unwrap();
        probe(control,&receipt,b"expected",128).unwrap();
        assert_eq!(std::fs::read_dir(control).unwrap().count(),0,"readiness leaked scratch or bookkeeping");
    }
}
