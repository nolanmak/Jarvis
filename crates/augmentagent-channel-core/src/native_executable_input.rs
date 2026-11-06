//! Descriptive original executable input binding; never source/body authority.
use std::{io::Read,os::unix::fs::{MetadataExt,OpenOptionsExt},path::{Path,PathBuf}};
use anyhow::{ensure,Context,Result};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
use super::{PreparedLaunchIntent,PreparedLaunchRecord};

pub(super) const MAX_EXECUTABLE_BYTES:u64=512*1024*1024;
/// Preserve the caller's host working-directory semantics before mapping its
/// invocation into the guest. PATH defaults remain the restricted native policy.
pub(super) fn resolve_executable(command:&std::process::Command)->Result<(PathBuf,PathBuf)> {
    let cwd=match command.get_current_dir() {
        Some(path) if path.is_absolute()=>path.to_path_buf(),
        Some(path)=>std::env::current_dir().context("native command working directory unavailable")?.join(path),
        None=>std::env::current_dir().context("native command working directory unavailable")?,
    };
    let cwd=cwd.canonicalize().context("native command working directory unavailable")?;
    ensure!(cwd.is_dir(),"native command working directory is not a directory");
    let raw=std::ffi::CString::new(cwd.as_os_str().as_encoded_bytes()).context("invalid native command working directory")?;
    ensure!(unsafe{libc::access(raw.as_ptr(),libc::X_OK)}==0,"native command working directory is not searchable");
    let program=Path::new(command.get_program());
    ensure!(!program.as_os_str().is_empty(),"native executable unavailable");
    if program.is_absolute() {return Ok((program.canonicalize()?,cwd));}
    if program.as_os_str().as_encoded_bytes().contains(&b'/') {return Ok((cwd.join(program).canonicalize()?,cwd));}
    let search=command.get_envs().find(|(key,_)|*key=="PATH").and_then(|(_,value)|value)
        .map(std::ffi::OsString::from).unwrap_or_else(||"/usr/bin:/bin".into());
    for directory in std::env::split_paths(&search) {
        let directory=if directory.is_absolute() {directory}else{cwd.join(directory)};
        let candidate=directory.join(program);
        if !candidate.is_file() {continue;}
        let raw=std::ffi::CString::new(candidate.as_os_str().as_encoded_bytes()).context("invalid native executable path")?;
        if unsafe{libc::access(raw.as_ptr(),libc::X_OK)}!=0 {continue;}
        return Ok((candidate.canonicalize()?,cwd));
    }
    anyhow::bail!("native executable unavailable")
}

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {device:u64,inode:u64,nlink:u64,mode:u32,uid:u32,gid:u32,bytes:u64,mtime:[i64;2],ctime:[i64;2]}
impl Origin {
    fn capture(info:&std::fs::Metadata)->Self {
        Self {device:info.dev(),inode:info.ino(),nlink:info.nlink(),mode:info.mode(),uid:info.uid(),gid:info.gid(),
            bytes:info.len(),mtime:[info.mtime(),info.mtime_nsec()],ctime:[info.ctime(),info.ctime_nsec()]}
    }
    fn validate(&self)->Result<()> {
        ensure!(self.mode&(libc::S_IFMT as u32)==libc::S_IFREG as u32 && (self.uid==0 || self.uid==unsafe{libc::geteuid()})
            && self.mode&0o022==0 && self.mode&0o111!=0 && self.nlink>0 && self.inode>0
            && (0..1_000_000_000).contains(&self.mtime[1]) && (0..1_000_000_000).contains(&self.ctime[1]),
            "native executable origin is not trusted");Ok(())
    }
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutableInput {version:u32,source:PathBuf,bytes:u64,sha256:String,origin:Origin}
impl ExecutableInput {
    pub(crate) fn validate(&self)->Result<()> {
        let path=self.source.to_str().context("native executable path is not UTF-8")?;
        ensure!(self.version==1 && self.source.is_absolute() && path.len()<=4096 && !path.as_bytes().contains(&0)
            && self.bytes>0 && self.bytes<=MAX_EXECUTABLE_BYTES && self.origin.bytes==self.bytes
            && self.sha256.len()==64 && self.sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),
            "native executable input manifest invalid");self.origin.validate()
    }
}
/// Stream the original descriptor before allocation. No second executable copy
/// or unregistered storage root is created; the existing guest copy is checked.
pub(super) fn capture_executable_input(source:&Path)->Result<ExecutableInput> {
    let mut file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(source)?;
    let before=file.metadata()?;let origin=Origin::capture(&before);origin.validate()?;
    ensure!(source.is_absolute() && before.is_file() && before.len()>0 && before.len()<=MAX_EXECUTABLE_BYTES,
        "native executable is not trusted or exceeds budget");
    let mut hash=Sha256::new();let mut total=0u64;let mut buffer=[0u8;65536];
    loop {let count=file.read(&mut buffer)?;if count==0 {break;}
        total=total.checked_add(count as u64).context("native executable size overflow")?;
        ensure!(total<=before.len(),"native executable changed during capture");hash.update(&buffer[..count]);}
    ensure!(total==before.len() && Origin::capture(&file.metadata()?)==origin,"native executable changed during capture");
    let captured=ExecutableInput {version:1,source:source.into(),bytes:total,sha256:format!("{:x}",hash.finalize()),origin};
    captured.validate()?;Ok(captured)
}
/// Strict saved history only; its constructor/serialization creates no permit.
#[derive(Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutableInputBindingRecord {
    pub(crate) version:u32,pub(crate) actor:String,pub(crate) kind:String,
    pub(crate) launch_intent:PreparedLaunchIntent,pub(crate) launch_receipt:crate::audit_chain::AppendReceipt,
    pub(crate) executable_input:ExecutableInput,
}
impl ExecutableInputBindingRecord {
    pub(crate) fn validate(&self,prepared:&PreparedLaunchRecord)->Result<()> {
        ensure!(self.version==1 && self.actor=="controller" && self.kind=="native_executable_input_binding"
            && self.launch_intent==prepared.intent && self.launch_receipt==prepared.controller_receipt,
            "native executable input original references changed");self.executable_input.validate()
    }
}
