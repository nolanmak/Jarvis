//! Physical checkout binding data for a saved owner preparation. This is not
//! repository authorization, native quiescence or compiled-source evidence.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, os::unix::fs::{MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Directory {
    path: PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
}
impl Directory {
    fn open(path: &Path) -> Result<(Self, File)> {
        ensure!(!std::fs::symlink_metadata(path)?.file_type().is_symlink(), "owner candidate directory is a symlink");
        let path=path.canonicalize()?;
        let file=std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path)?;
        let metadata=file.metadata()?;
        ensure!(metadata.is_dir() && metadata.uid()==unsafe{libc::geteuid()}, "owner candidate directory is not owned");
        Ok((Self {path,device:metadata.dev(),inode:metadata.ino(),uid:metadata.uid()},file))
    }
    fn verify(&self) -> Result<()> {
        ensure!(Self::open(&self.path)?.0==*self,"owner candidate directory changed");Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pointer {
    path: PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
    sha256: String,
}
impl Pointer {
    fn read(path: &Path) -> Result<(Self, String)> {
        let mut file=std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
        let before=file.metadata()?;
        ensure!(before.is_file() && before.nlink()==1 && before.uid()==unsafe{libc::geteuid()}
            && before.len()>0 && before.len()<=4096,"invalid owner candidate Git pointer");
        let mut bytes=Vec::new();(&mut file).take(4097).read_to_end(&mut bytes)?;
        let after=file.metadata()?;
        ensure!(bytes.len() as u64==before.len() && before.len()==after.len()
            && before.mtime()==after.mtime() && before.mtime_nsec()==after.mtime_nsec()
            && before.ctime()==after.ctime() && before.ctime_nsec()==after.ctime_nsec(),
            "owner candidate Git pointer changed while reading");
        let text=std::str::from_utf8(&bytes)?.trim_end_matches('\n').to_owned();
        ensure!(!text.is_empty() && !text.contains(['\0','\n','\r']),"invalid owner candidate Git pointer contents");
        Ok((Self {path:path.to_path_buf(),device:before.dev(),inode:before.ino(),uid:before.uid(),
            sha256:format!("{:x}",Sha256::digest(&bytes))},text))
    }
    fn verify(&self) -> Result<()> {
        ensure!(Self::read(&self.path)?.0==*self,"owner candidate Git pointer changed");Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CandidateIdentity {
    version: u32,
    root: Directory,
    git_pointer: Option<Pointer>,
    git_directory: Directory,
    common_pointer: Option<Pointer>,
    common_directory: Directory,
}
impl CandidateIdentity {
    pub(super) fn capture(candidate: &Path) -> Result<Self> {
        let (root,_root_fd)=Directory::open(candidate)?;
        let git_path=root.path.join(".git");
        let metadata=std::fs::symlink_metadata(&git_path)?;
        let (git_pointer,git_path)=if metadata.is_dir() {(None,git_path)} else {
            let (pointer,text)=Pointer::read(&git_path)?;
            let target=text.strip_prefix("gitdir: ").context("owner candidate Git pointer prefix missing")?;
            ensure!(!target.is_empty(),"owner candidate Git directory missing");
            (Some(pointer),root.path.join(target))
        };
        let (git_directory,_git_fd)=Directory::open(&git_path)?;
        let common_path=git_directory.path.join("commondir");
        let (common_pointer,common_path)=match std::fs::symlink_metadata(&common_path) {
            Ok(_)=>{let (pointer,target)=Pointer::read(&common_path)?;(Some(pointer),git_directory.path.join(target))},
            Err(error) if error.kind()==std::io::ErrorKind::NotFound => (None,git_directory.path.clone()),
            Err(error)=>return Err(error.into()),
        };
        let (common_directory,_common_fd)=Directory::open(&common_path)?;
        let identity=Self {version:1,root,git_pointer,git_directory,common_pointer,common_directory};
        identity.root.verify()?;identity.git_directory.verify()?;identity.common_directory.verify()?;
        for pointer in [&identity.git_pointer,&identity.common_pointer].into_iter().flatten() {pointer.verify()?;}
        Ok(identity)
    }
    pub(super) fn verify(&self,candidate: &Path) -> Result<()> {
        ensure!(*self==Self::capture(candidate)?,"owner preparation physical candidate changed");Ok(())
    }
    pub(super) fn verify_current(&self) -> Result<()> {self.verify(&self.root.path)}
    pub(super) fn root_path(&self)->&Path {&self.root.path}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root:&Path,args:&[&str])->String {
        let output=std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn linked()->(tempfile::TempDir,PathBuf) {
        let root=tempfile::tempdir().unwrap();git(root.path(),&["init","-q"]);
        std::fs::write(root.path().join("behavior.txt"),"original\n").unwrap();git(root.path(),&["add","behavior.txt"]);
        git(root.path(),&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","baseline"]);
        let candidate=root.path().join("candidate");
        git(root.path(),&["worktree","add","--detach",candidate.to_str().unwrap(),"HEAD"]);(root,candidate)
    }
    #[test]
    fn linked_checkout_identity_survives_reopen_and_normal_product_edits() {
        let (_root,candidate)=linked();let identity=CandidateIdentity::capture(&candidate).unwrap();
        let restored:CandidateIdentity=serde_json::from_slice(&serde_json::to_vec(&identity).unwrap()).unwrap();
        assert_eq!(restored,identity);restored.verify(&candidate).unwrap();
        std::fs::write(candidate.join("behavior.txt"),"owner's continued work\n").unwrap();
        restored.verify(&candidate).unwrap();
        // Physical identity does not certify source content or its freshness.
        assert_ne!(git(&candidate,&["diff","--","behavior.txt"]),"");
    }
    #[test]
    fn linked_checkout_rejects_substituted_git_and_common_directories() {
        let (_root,candidate)=linked();let identity=CandidateIdentity::capture(&candidate).unwrap();
        let head=git(&candidate,&["rev-parse","HEAD"]);
        for source in [&identity.git_directory.path,&identity.common_directory.path] {
            let retained=tempfile::tempdir().unwrap();let original=retained.path().join("original");
            std::fs::rename(source,&original).unwrap();
            assert!(std::process::Command::new("cp").arg("-a").arg(&original).arg(source).status().unwrap().success());
            assert_eq!(git(&candidate,&["rev-parse","HEAD"]),head);
            assert!(identity.verify(&candidate).is_err(),"copied Git metadata replaced the bound physical identity");
            std::fs::remove_dir_all(source).unwrap();std::fs::rename(&original,source).unwrap();
            identity.verify(&candidate).unwrap();
        }
    }
    #[test]
    fn linked_git_pointers_refuse_replacement_aliases_and_nonregular_files() {
        let (_root,candidate)=linked();let identity=CandidateIdentity::capture(&candidate).unwrap();
        for pointer in [&identity.git_pointer,&identity.common_pointer].into_iter().flatten() {
            for fault in ["copy","symlink","hardlink","fifo","changed-bytes"] {
                let retained=tempfile::tempdir().unwrap();let original=retained.path().join("original");
                std::fs::rename(&pointer.path,&original).unwrap();
                match fault {
                    "copy"=>{std::fs::copy(&original,&pointer.path).unwrap();},
                    "symlink"=>std::os::unix::fs::symlink(&original,&pointer.path).unwrap(),
                    "hardlink"=>std::fs::hard_link(&original,&pointer.path).unwrap(),
                    "fifo"=>{
                        use std::os::unix::ffi::OsStrExt;
                        let name=std::ffi::CString::new(pointer.path.as_os_str().as_bytes()).unwrap();
                        assert_eq!(unsafe{libc::mkfifo(name.as_ptr(),0o600)},0);
                    },
                    "changed-bytes"=>{std::fs::copy(&original,&pointer.path).unwrap();std::fs::write(&pointer.path,b"gitdir: /missing-owner-fixture\n").unwrap();},
                    _=>unreachable!(),
                }
                let checked=identity.clone();let checked_candidate=candidate.clone();let (sent,received)=std::sync::mpsc::channel();
                let thread=std::thread::spawn(move||sent.send(checked.verify(&checked_candidate).is_err()).unwrap());
                assert!(received.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),"{fault}: invalid Git pointer accepted");
                thread.join().unwrap();std::fs::remove_file(&pointer.path).unwrap();std::fs::rename(&original,&pointer.path).unwrap();
                identity.verify(&candidate).unwrap();
            }
        }
    }
}
