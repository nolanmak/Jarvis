//! Resolve once, observe without applying Git URL rewrites a second time, and
//! fence the actual Git transport before it can send a pack.
use super::*;
use anyhow::ensure;
use std::os::unix::fs::PermissionsExt;

pub(super) struct Resolved {
    destination:String,
}
pub(super) struct Hook {
    _directory:tempfile::TempDir,
    config:String,
}
impl Hook {
    pub(super) fn config(&self)->&str {&self.config}
}
impl Resolved {
    pub(super) async fn resolve(root:&Path,repository:&str)->Result<Self> {
        let (ok,raw,_)=run("git",&["remote","get-url","--push","--all","origin"],root).await?;
        ensure!(ok && raw.len()<=16384,"Push destination unavailable");
        let destinations=raw.lines().collect::<Vec<_>>();
        let [destination]=destinations.as_slice() else {anyhow::bail!("Push requires one unambiguous destination")};
        check_destination(repository,destination)?;
        Ok(Self {destination:(*destination).to_owned()})
    }
    pub(super) fn destination(&self)->&str {&self.destination}
    pub(super) async fn head(&self,root:&Path,repository:&str,reference:&str)->Result<Option<String>> {
        head_at(root,repository,&self.destination,reference).await
    }

    pub(super) async fn hook(&self,root:&Path,private_parent:&Path)->Result<Hook> {
        if configured_ccat_public_push(&["push","origin"]) {
            require_ccat_pre_push_hook(root).map_err(anyhow::Error::msg)?;
        }
        let (ok,path,_)=run("git",&["rev-parse","--path-format=absolute","--git-path","hooks/pre-push"],root).await?;
        ensure!(ok && !path.trim().is_empty(),"original pre-push hook location unavailable");
        let original=PathBuf::from(path.trim());
        let original=match std::fs::metadata(&original) {
            Ok(metadata) if metadata.is_file() && metadata.permissions().mode()&0o111!=0=>Some(original),
            Ok(_)=>None,
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>None,
            Err(error)=>return Err(anyhow::Error::new(error).context("original pre-push hook unavailable")),
        };
        let directory=tempfile::Builder::new().prefix("push-destination-").tempdir_in(private_parent)?;
        // Git propagates our -c hooksPath into hook subprocesses. The original
        // hook must see exactly its parent's configuration instead, including
        // any pre-existing command configuration, not this temporary wrapper.
        let configuration=match std::env::var("GIT_CONFIG_PARAMETERS") {
            Ok(value)=>format!("export GIT_CONFIG_PARAMETERS={}\n",quote(&value)),
            Err(std::env::VarError::NotPresent)=>"unset GIT_CONFIG_PARAMETERS\n".into(),
            Err(error)=>return Err(error.into()),
        };
        let continuation=match original {
            Some(path)=>format!("exec {} \"$@\"\n",quote(path.to_str().context("hook location is not UTF-8")?)),
            None=>"exit 0\n".into(),
        };
        let script=format!("#!/bin/sh\n[ \"$#\" -eq 2 ] && [ \"$2\" = {} ] || exit 1\n{}{}",quote(&self.destination),configuration,continuation);
        let hook=directory.path().join("pre-push");
        std::fs::write(&hook,script)?;
        std::fs::set_permissions(&hook,std::fs::Permissions::from_mode(0o700))?;
        let config=format!("core.hooksPath={}",directory.path().to_str().context("private hook location is not UTF-8")?);
        Ok(Hook {_directory:directory,config})
    }
}
fn quote(value:&str)->String {format!("'{}'",value.replace('\'',"'\\''"))}

fn check_destination(repository:&str,destination:&str)->Result<Option<PathBuf>> {
    ensure!(!destination.is_empty() && destination.len()<=16384
        && !destination.bytes().any(|byte|byte.is_ascii_control()),"invalid Push destination envelope");
    if let Some(identity)=crate::wiki_sync_remote(destination)? {
        ensure!(identity.eq_ignore_ascii_case(repository),"Push destination differs from protected repository");
        return Ok(None);
    }
    let path=if destination.starts_with("file://") {
        reqwest::Url::parse(destination)?.to_file_path().map_err(|_|anyhow::anyhow!("local Push destination malformed"))?
    }else {PathBuf::from(destination)};
    ensure!(path.is_absolute(),"local Push destination must be absolute");
    let canonical=path.canonicalize().context("local Push destination unavailable")?;
    ensure!(canonical.is_dir(),"local Push destination is not a repository directory");
    Ok(Some(canonical))
}

/// Read-only data inspection, not a dispatch capability. Network refs use the
/// authenticated GitHub identity; local mirrors read their actual Git metadata.
pub(super) async fn head_at(root:&Path,repository:&str,destination:&str,reference:&str)->Result<Option<String>> {
    let (valid,_,_)=run("git",&["check-ref-format",reference],root).await?;
    ensure!(valid,"Push reference malformed");
    if let Some(path)=check_destination(repository,destination)? {
        let (opened,git_directory,_)=run("git",&["-C",path.to_str().context("local Push destination is not UTF-8")?,
            "rev-parse","--absolute-git-dir"],root).await?;
        ensure!(opened && !git_directory.trim().is_empty(),"local Push repository metadata unavailable");
        let directory=format!("--git-dir={}",git_directory.trim());
        let (ok,out,err)=run("git",&[&directory,"rev-parse","--verify","--quiet",reference],root).await?;
        if !ok && out.is_empty() && err.is_empty() {return Ok(None);}
        ensure!(ok,"local Push destination observation unavailable");
        let head=out.trim();
        ensure!(matches!(head.len(),40|64) && head.bytes().all(|b|b.is_ascii_hexdigit()),"local Push observation malformed");
        Ok(Some(head.into()))
    }else {
        publication::read_bound_push_head(root,destination,reference).await
    }
}
