//! Original raw output captured by the trusted supervisor, outside worker writes.
use super::*;
use std::os::unix::fs::{DirBuilderExt,MetadataExt,OpenOptionsExt};

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Capture {
    path:PathBuf,device:u64,inode:u64,
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {version:u32,command:String,output_limit:u64}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version:u32,command:String,identity:serde_json::Value,exit_code:Option<i32>,
    interrupted:bool,output_complete:bool,terminal_cause:String,
    stdout_bytes:u64,stderr_bytes:u64,stdout_sha256:String,stderr_sha256:String,
}

pub(super) struct Recovered {
    pub(super) observation:Observation,
    pub(super) manifest_sha256:String,
}

fn private_directory(path:&Path)->Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(())=>(),Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>(),Err(error)=>return Err(error.into()),
    }
    let metadata=std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid()==unsafe{libc::geteuid()}
        && metadata.mode()&0o077==0 && path.canonicalize()?==path,"raw capture directory is not private canonical controller state");
    Ok(())
}

impl Capture {
    pub(super) fn prepare(control:&Path,worker:&Path,command:&str)->Result<Self> {
        use std::io::Write;
        ensure!(command.len()==64 && command.bytes().all(|c|c.is_ascii_hexdigit()),"invalid original command identity");
        let control=control.canonicalize()?;
        for root in worker_roots(worker) {
            let root=root.canonicalize()?;
            ensure!(!control.starts_with(&root) && !root.starts_with(&control),"worker overlaps raw result storage");
        }
        let parent=control.join("verifier-command-results");private_directory(&parent)?;
        let path=parent.join(command);private_directory(&path)?;
        let metadata=std::fs::symlink_metadata(&path)?;
        let request=serde_json::to_vec(&Request {version:1,command:command.into(),output_limit:OUTPUT_LIMIT})?;
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path.join("request.json")) {
            Ok(mut file)=>{file.write_all(&request)?;file.sync_all()?;std::fs::File::open(&path)?.sync_all()?;},
            Err(error) if error.kind()==std::io::ErrorKind::AlreadyExists=>{
                ensure!(baseline_witness::private_bytes(&path.join("request.json"),4096)?==request,"original capture request changed");
            },
            Err(error)=>return Err(error.into()),
        }
        Ok(Self {path,device:metadata.dev(),inode:metadata.ino()})
    }

    pub(super) fn path(&self)->&Path {&self.path}

    pub(super) fn read(&self,command:&str,identity:&serde_json::Value)->Result<Option<Recovered>> {
        let metadata=std::fs::symlink_metadata(&self.path)?;
        ensure!(metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid()==unsafe{libc::geteuid()}
            && metadata.mode()&0o077==0 && metadata.dev()==self.device && metadata.ino()==self.inode
            && self.path.canonicalize()?==self.path,"original capture directory identity changed");
        let request:Request=serde_json::from_slice(&baseline_witness::private_bytes(&self.path.join("request.json"),4096)?)?;
        ensure!(request.version==1 && request.command==command && request.output_limit==OUTPUT_LIMIT,"original capture request differs");
        let result=self.path.join("result.json");
        if matches!(std::fs::symlink_metadata(&result),Err(ref error) if error.kind()==std::io::ErrorKind::NotFound) {return Ok(None);}
        let raw=baseline_witness::private_bytes(&result,4096)?;
        let manifest:Manifest=serde_json::from_slice(&raw)?;
        ensure!(manifest.version==1 && manifest.command==command && &manifest.identity==identity,"raw result belongs to another command or resource scope");
        ensure!(matches!(manifest.terminal_cause.as_str(),"completed"|"controller_stopped"|"output_limit"|"resource_exhausted"|"descendants_killed"|"signalled"|"launch_failed"|"cancelled_before_dispatch"),"unknown raw result cause");
        ensure!(manifest.interrupted || (manifest.output_complete && manifest.exit_code.is_some_and(|code|(0..=255).contains(&code))
            && manifest.terminal_cause=="completed"),"incomplete raw result claims normal completion");
        let stdout=baseline_witness::private_bytes(&self.path.join("stdout"),OUTPUT_LIMIT as usize)?;
        let stderr=baseline_witness::private_bytes(&self.path.join("stderr"),OUTPUT_LIMIT as usize)?;
        ensure!(stdout.len() as u64==manifest.stdout_bytes && stderr.len() as u64==manifest.stderr_bytes
            && amendment::digest(&stdout)==manifest.stdout_sha256 && amendment::digest(&stderr)==manifest.stderr_sha256,"original raw output changed");
        Ok(Some(Recovered {manifest_sha256:amendment::digest(&raw),observation:Observation {
            exit_code:manifest.exit_code,stdout:String::from_utf8_lossy(&stdout).into_owned(),stderr:String::from_utf8_lossy(&stderr).into_owned(),
            interrupted:manifest.interrupted,output_complete:manifest.output_complete,
        }}))
    }
}
