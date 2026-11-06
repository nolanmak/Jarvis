//! Descriptive original executable input binding; never source/body authority.
use std::{io::Read,os::unix::fs::{MetadataExt,OpenOptionsExt},path::{Path,PathBuf}};
use anyhow::{ensure,Context,Result};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
use super::{PreparedLaunchIntent,PreparedLaunchRecord};

pub(super) const MAX_EXECUTABLE_BYTES:u64=512*1024*1024;
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
