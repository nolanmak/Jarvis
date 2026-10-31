//! Auto-ship's process lifetime is independent of the serving daemon. The
//! worker finishes its current tick before loading a replacement executable.
use anyhow::{Context, Result};
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
}
type ConfigurationStamp=(u64,u64,u64,i64,i64,i64,i64);
fn configuration_stamp(path:&Path)->Result<Option<ConfigurationStamp>> {
    match std::fs::metadata(path) {
        Ok(m)=>Ok(Some((m.dev(),m.ino(),m.len(),m.mtime(),m.mtime_nsec(),m.ctime(),m.ctime_nsec()))),
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),
        Err(error)=>Err(error.into()),
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
        Ok(Self {path,identity: (metadata.dev(), metadata.ino()),configuration})
    }
    pub fn changed(&self) -> Result<bool> {
        for (path,stamp) in &self.configuration {
            if &configuration_stamp(path)?!=stamp {return Ok(true);}
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
        }
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
    match worker
        .with_upgrade_watch(UpgradeWatch::current()?)
        .run(shutdown)
        .await
    {
        Err(e) if e.downcast_ref::<UpgradeRequested>().is_some() => {
            tracing::info!("{e}");
            // The dedicated service treats this as an expected, forced
            // restart. Disabled opt-in still returns zero and stays stopped.
            std::process::exit(75)
        }
        result => result,
    }
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
}
