//! Auto-ship's process lifetime is independent of the serving daemon. The
//! worker finishes its current tick before loading a replacement executable.
//! A new executable, configuration or updater build stamp (#2303) is noticed
//! within seconds while idle; the worker then exits 75 for its supervisor.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct UpgradeRequested;
impl std::fmt::Display for UpgradeRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "auto-ship worker reached a boundary; loading the updated executable"
        )
    }
}
impl std::error::Error for UpgradeRequested {}

pub struct UpgradeWatch {
    path: PathBuf,
    identity: (u64, u64),
    configuration:Vec<(PathBuf,Option<ConfigurationStamp>)>,
    /// #2303: the updater's build stamp and the commit it named at startup.
    deployment:Option<(PathBuf,Option<String>)>,
}
type ConfigurationStamp=(u64,u64,u64,String);
fn configuration_stamp(path:&Path)->Result<Option<ConfigurationStamp>> {
    let m=match std::fs::metadata(path) {
        Ok(m)=>m,
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),
        Err(error)=>return Err(error.into()),
    };
    // Inode timestamps come from a coarse clock, so an editor rewriting a
    // policy in place with the same length can leave every one of them
    // identical: only the contents witness a different configuration. Stamping
    // the digest instead also keeps a `touch` from forcing a needless restart.
    let content=if m.is_file() {
        match std::fs::read(path) {
            Ok(bytes)=>format!("{:x}",Sha256::digest(bytes)),
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),
            Err(error)=>return Err(error.into()),
        }
    } else {String::new()};
    Ok(Some((m.dev(),m.ino(),m.len(),content)))
}
/// The commit the updater last deployed (`built-commit`). It is written only
/// after a verified deploy, so it is the deploy signal even when the watched
/// executable path was not replaced. Absent, empty or unreadable names no
/// deploy: a withdrawn stamp is never a reason to restart.
fn deployment_stamp(path:&Path)->Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text)=>Some(text.trim().to_string()).filter(|commit|!commit.is_empty()),
        Err(error)=>{
            if error.kind()!=std::io::ErrorKind::NotFound {
                tracing::warn!(path=%path.display(),"build stamp unreadable; not treated as a deploy: {error}");
            }
            None
        }
    }
}
impl UpgradeWatch {
    pub fn current() -> Result<Self> {
        let path = std::env::current_exe()?;
        let path = PathBuf::from(path.to_string_lossy().trim_end_matches(" (deleted)"));
        #[cfg(target_os = "linux")]
        let metadata = std::fs::metadata("/proc/self/exe")?;
        #[cfg(not(target_os = "linux"))]
        let metadata = std::fs::metadata(&path)?;
        let config=std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(||
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
        let mut paths=vec![config.join("augmentagent/autoship-native.env")];
        if let Some(policy)=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY") {paths.push(PathBuf::from(policy));}
        let configuration=paths.into_iter().map(|path|Ok((path.clone(),configuration_stamp(&path)?))).collect::<Result<Vec<_>>>()?;
        // #2303: record the deploy this process started under, so only a
        // later deploy (never the same stamp) requests a reload.
        let deployment=crate::build_storage::state_dir().ok().map(|state|{
            let stamp=state.join("built-commit");let seen=deployment_stamp(&stamp);(stamp,seen)});
        Ok(Self {path,identity: (metadata.dev(), metadata.ino()),configuration,deployment})
    }
    pub fn changed(&self) -> Result<bool> {
        for (path,stamp) in &self.configuration {
            if &configuration_stamp(path)?!=stamp {return Ok(true);}
        }
        if let Some((stamp,seen))=&self.deployment {
            if deployment_stamp(stamp).is_some_and(|commit|seen.as_deref()!=Some(commit.as_str())) {
                return Ok(true);
            }
        }
        match std::fs::metadata(&self.path) {
            Ok(m) => Ok(m.is_file() && (m.dev(), m.ino()) != self.identity),
            // A failed build/removal is not a new executable to run.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(test)]
    pub fn at(path: &Path) -> Self {
        let m = std::fs::metadata(path).unwrap();
        Self {
            path: path.into(),
            identity: (m.dev(), m.ino()),
            configuration:Vec::new(),
            deployment:None,
        }
    }
    #[cfg(test)]
    pub fn with_deployment_stamp(mut self, stamp: &Path) -> Self {
        self.deployment=Some((stamp.into(),deployment_stamp(stamp)));
        self
    }
}

/// The supervisor restarts the worker onto the deployed executable for this
/// status (`RestartForceExitStatus=75`).
pub const RESTART_EXIT_CODE: i32 = 75;

pub(crate) fn restart_code(result: &Result<()>) -> Option<i32> {
    match result {
        Err(error) if error.downcast_ref::<UpgradeRequested>().is_some() => Some(RESTART_EXIT_CODE),
        _ => None,
    }
}

pub fn external_enabled() -> bool {
    cfg!(target_os = "linux")
        && matches!(
            std::env::var("AUGMENTAGENT_AUTOPR_EXTERNAL").as_deref(),
            Ok("1" | "true")
        )
}

/// This is written by the actual daemon, not by the installer. The updater
/// must not mistake a configured-but-not-yet-restarted legacy daemon for an
/// isolated worker deployment.
pub fn record_host_mode(external: bool) -> Result<()> {
    let state = crate::build_storage::state_dir()?;
    std::fs::create_dir_all(&state)?;
    let birth = process_birth(Path::new("/proc/self/stat")).unwrap_or_default();
    let value =
        serde_json::json!({"pid": std::process::id(), "birth": birth, "external": external});
    let temporary = state.join(format!("autopr-host.{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&value)?)?;
    std::fs::rename(temporary, state.join("autopr-host.json"))?;
    Ok(())
}

fn process_birth(stat: &Path) -> Result<String> {
    let text = std::fs::read_to_string(stat)?;
    Ok(text
        .rsplit_once(')')
        .context("invalid process identity")?
        .1
        .split_whitespace()
        .nth(19)
        .context("missing process birth")?
        .to_string())
}

pub async fn run(repo: PathBuf, dry_run: bool) -> Result<()> {
    if let Err(error)=crate::autoship_deployment::record("worker", dry_run, true).await {
        tracing::warn!("worker configuration receipt unavailable; productive preflight remains blocked: {error:#}");
    }
    let Some(worker) = crate::self_improve::AutoPrLoop::from_env(repo, dry_run) else {
        tracing::info!("auto-ship worker disabled: AUGMENTAGENT_AUTOPR is not enabled");
        return Ok(());
    };
    let shutdown = tokio_util::sync::CancellationToken::new();
    let cancelled = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        cancelled.cancel();
    });
    let result = worker
        .with_upgrade_watch(UpgradeWatch::current()?)
        .run(shutdown)
        .await;
    if let (Some(code), Err(e)) = (restart_code(&result), &result) {
        tracing::info!("{e}");
        // The dedicated service treats this as an expected, forced
        // restart. Disabled opt-in still returns zero and stays stopped.
        std::process::exit(code)
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn disabled_worker_exits_cleanly_even_when_receipt_storage_is_unavailable() {
        const CHILD:&str="AUTOSHIP_DISABLED_RECEIPT_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let root=tempfile::tempdir().unwrap();let blocked=root.path().join("file");std::fs::write(&blocked,"not a state directory").unwrap();
            let result=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","autoship_worker::tests::disabled_worker_exits_cleanly_even_when_receipt_storage_is_unavailable"])
                .env_clear().env("PATH","/usr/bin:/bin").env("HOME",root.path()).env("XDG_STATE_HOME",&blocked).env(CHILD,"1")
                .output().unwrap();
            assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stdout));return;
        }
        assert!(run(std::env::current_dir().unwrap(),false).await.is_ok());
    }
    #[test]
    fn policy_selection_and_in_place_updates_request_boundary_reload() {
        let root=tempfile::tempdir().unwrap();let binary=root.path().join("binary");std::fs::write(&binary,"same executable").unwrap();
        for name in ["selection.env","policy.json"] {
            let path=root.path().join(name);std::fs::write(&path,"old").unwrap();
            let mut watch=UpgradeWatch::at(&binary);watch.configuration.push((path.clone(),configuration_stamp(&path).unwrap()));
            assert!(!watch.changed().unwrap());std::fs::write(&path,"new").unwrap();assert!(watch.changed().unwrap());
        }
    }
    /// The kernel stamps inodes from a coarse clock, so two same-length writes
    /// inside one tick leave the metadata identical and only the contents
    /// witness the new policy; restoring the modification time reproduces that
    /// metadata exactly. A bare `touch` is the same invariant from the other
    /// side: it is not a policy the worker has to reload for.
    #[test]
    fn a_policy_reload_follows_the_contents_not_the_inode_timestamps() {
        let root=tempfile::tempdir().unwrap();let binary=root.path().join("binary");
        std::fs::write(&binary,"same executable").unwrap();
        let path=root.path().join("autoship-native.env");std::fs::write(&path,"old").unwrap();
        let before=std::fs::metadata(&path).unwrap();
        let watch=||{let mut w=UpgradeWatch::at(&binary);
            w.configuration.push((path.clone(),configuration_stamp(&path).unwrap()));w};
        let set_modified=|at|std::fs::File::options().write(true).open(&path).unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(at)).unwrap();
        let rewritten=watch();
        std::fs::write(&path,"new").unwrap();set_modified(before.modified().unwrap());
        let after=std::fs::metadata(&path).unwrap();
        assert_eq!((after.len(),after.ino(),after.modified().unwrap()),
            (before.len(),before.ino(),before.modified().unwrap()),"fixture must replay the coarse clock");
        assert!(rewritten.changed().unwrap(),"a same-length in-place rewrite is a new policy");
        let touched=watch();
        set_modified(before.modified().unwrap()+std::time::Duration::from_secs(60));
        assert!(!touched.changed().unwrap(),"a touch alone is not a new policy");
    }
    #[test]
    fn replacing_binary_is_detected_without_treating_missing_file_as_an_upgrade() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("worker");
        std::fs::write(&file, "old").unwrap();
        let watch = UpgradeWatch::at(&file);
        let _executing = std::fs::File::open(&file).unwrap();
        assert!(!watch.changed().unwrap());
        std::fs::remove_file(&file).unwrap();
        assert!(!watch.changed().unwrap());
        let replacement = t.path().join("replacement");
        std::fs::write(&replacement, "new").unwrap();
        std::fs::rename(replacement, &file).unwrap();
        assert!(watch.changed().unwrap());
    }
    /// #2303: the updater writes `built-commit` only after a verified
    /// deploy. That stamp is the deploy signal even when the executable path
    /// the watch follows was not replaced.
    #[test]
    fn a_new_deployment_stamp_requests_a_reload_with_the_binary_unchanged() {
        let t=tempfile::tempdir().unwrap();
        let binary=t.path().join("worker");std::fs::write(&binary,"same executable").unwrap();
        let stamp=t.path().join("built-commit");std::fs::write(&stamp,"aaaa\n").unwrap();
        let watch=UpgradeWatch::at(&binary).with_deployment_stamp(&stamp);
        assert!(!watch.changed().unwrap(),"the stamp this process started under is not a deploy");
        std::fs::write(&stamp,"bbbb\n").unwrap();
        assert!(watch.changed().unwrap(),"a new build stamp is a deploy to load");
        // A first deploy on a fresh host: no stamp at startup, then one.
        let fresh=t.path().join("fresh-stamp");
        let watch=UpgradeWatch::at(&binary).with_deployment_stamp(&fresh);
        assert!(!watch.changed().unwrap());
        std::fs::write(&fresh,"cccc\n").unwrap();
        assert!(watch.changed().unwrap(),"the first stamp ever written is a deploy too");
    }
    /// The replacement process records the stamp it started under, so the
    /// same deploy can never restart it again: rewriting the same commit
    /// (even as a new inode), a bare touch, withdrawal or truncation is not
    /// a deploy.
    #[test]
    fn an_unchanged_or_withdrawn_stamp_never_requests_a_reload() {
        let t=tempfile::tempdir().unwrap();
        let binary=t.path().join("worker");std::fs::write(&binary,"same executable").unwrap();
        let stamp=t.path().join("built-commit");std::fs::write(&stamp,"aaaa\n").unwrap();
        let watch=UpgradeWatch::at(&binary).with_deployment_stamp(&stamp);
        let rewritten=t.path().join("stamp.tmp");std::fs::write(&rewritten,"aaaa").unwrap();
        std::fs::rename(&rewritten,&stamp).unwrap();
        assert!(!watch.changed().unwrap(),"the same commit rewritten is not a new deploy");
        std::fs::File::options().write(true).open(&stamp).unwrap()
            .set_modified(std::time::SystemTime::now()+std::time::Duration::from_secs(60)).unwrap();
        assert!(!watch.changed().unwrap(),"a touch is not a deploy");
        std::fs::write(&stamp,"").unwrap();
        assert!(!watch.changed().unwrap(),"a truncated stamp names no deploy");
        std::fs::remove_file(&stamp).unwrap();
        assert!(!watch.changed().unwrap(),"a withdrawn stamp names no deploy");
    }
    /// The process exit status is the supervisor contract: only a reload
    /// boundary maps to the forced-restart status.
    #[test]
    fn only_a_reload_boundary_exits_with_the_restart_status() {
        assert_eq!(restart_code(&Err(UpgradeRequested.into())),Some(75));
        assert_eq!(restart_code(&Err(anyhow::Error::new(UpgradeRequested).context("tick"))),Some(75));
        assert_eq!(restart_code(&Ok(())),None,"operator shutdown stays stopped");
        assert_eq!(restart_code(&Err(anyhow::anyhow!("tick failed"))),None,"a fault is not a reload");
    }
}
