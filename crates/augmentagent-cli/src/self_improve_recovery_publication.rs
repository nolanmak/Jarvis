//! Controller filesystem readiness. Never publishes an acceptance artifact.
use super::*;

pub(super) fn probe(control:&Path,receipt:&Path,expected:&[u8],additional_bytes:usize)->Result<()> {
    probe_with(control,receipt,expected,additional_bytes,|file,length| {
        use std::io::Write;
        let block=[0u8;65536];let mut remaining=length;
        while remaining>0 {let count=remaining.min(block.len());file.write_all(&block[..count])?;remaining-=count;}
        file.sync_all()?;Ok(())
    })
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
    // Anonymous scratch has no durable name, including after process death.
    // This probes actual allocation/quota/write/fsync, never a receipt grant.
    let mut scratch=anonymous(&control)?;
    write(&mut scratch,expected.len()+additional_bytes+64*1024)?;
    drop(scratch);std::fs::File::open(&control)?.sync_all()?;Ok(())
}

fn anonymous(control:&Path)->Result<std::fs::File> {
    #[cfg(target_os="linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(std::fs::OpenOptions::new().read(true).write(true).mode(0o600)
            .custom_flags(libc::O_TMPFILE|libc::O_CLOEXEC).open(control)?)
    }
    #[cfg(not(target_os="linux"))]
    {let _=control;anyhow::bail!("anonymous controller publication probe unavailable on this platform")}
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn publication_probe_preserves_receipts_and_cleans_up_write_failure() {
        let root=tempfile::tempdir().unwrap();let receipt=root.path().join("receipt.json");
        std::fs::set_permissions(root.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&receipt,b"expected").unwrap();
        std::fs::set_permissions(&receipt,std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(probe_with(root.path(),&receipt,b"expected",128,|_,_|Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into())).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(),1);
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
        assert_eq!(std::fs::read_dir(control).unwrap().count(),0,"readiness published an artifact or leaked scratch");
    }
}
