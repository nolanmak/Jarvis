//! Auto-ship's process lifetime is independent of the serving daemon. The
//! worker finishes its current tick before loading a replacement executable.
//! A new executable, configuration or updater build stamp (#2303) is noticed
//! within seconds while idle; the worker then exits 75 for its supervisor.
//! A busy worker restarts at the next safe point inside its tick (#2354).
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
/// #2468: set on a pinned worker's unit (scripts/lib/autoship-pin.sh). It
/// names the stamp of the pinned build, which changes only when the owner
/// moves the pin, so merges to main no longer reload the worker.
pub const DEPLOY_STAMP_ENV: &str = "AUGMENTAGENT_AUTOSHIP_DEPLOY_STAMP";
/// The deploy stamp this worker reloads on: the pinned stamp when one is
/// configured, otherwise the updater's `built-commit` (unpinned, #2303).
fn deployment_stamp_path(pinned:Option<std::ffi::OsString>,state:Option<PathBuf>)->Option<PathBuf> {
    match pinned.filter(|path|!path.is_empty()) {
        Some(path)=>Some(PathBuf::from(path)),
        None=>state.map(|state|state.join("built-commit")),
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
        // later deploy (never the same stamp) requests a reload. #2468: a
        // pinned worker watches its own install stamp instead of main's.
        let deployment=deployment_stamp_path(std::env::var_os(DEPLOY_STAMP_ENV),
            crate::build_storage::state_dir().ok()).map(|stamp|{let seen=deployment_stamp(&stamp);(stamp,seen)});
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

/// #2354: how long a deploy may wait for a safe point before the worker stops
/// claiming new work in its concurrent lanes and drains toward the restart.
pub const STALE_DEPLOY_BOUND: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// #2354: a tick can chain scope, build, verification and repair for hours,
/// so waiting for the tick boundary ran production on hours-stale code. The
/// worker runs each tick inside these safe points. Code that has just durably
/// written a workflow checkpoint, or is about to claim the next workflow,
/// calls [`safe_point`]; when a deploy is pending and no build or verifier of
/// this tick is in flight, the tick parks there and the worker drops it and
/// exits 75. Everything the tick did up to that point is already durable.
pub struct SafePoints {
    watch: std::sync::Arc<UpgradeWatch>,
    busy: std::sync::atomic::AtomicUsize,
    parked: tokio_util::sync::CancellationToken,
    pending_since: std::sync::Mutex<Option<tokio::time::Instant>>,
    drain: tokio_util::sync::CancellationToken,
}
tokio::task_local! {
    static SAFE_POINTS: std::sync::Arc<SafePoints>;
}
impl SafePoints {
    /// `drain` is cancelled once a deploy has been pending past
    /// [`STALE_DEPLOY_BOUND`]; the worker's concurrent lanes stop claiming then.
    pub fn new(watch: std::sync::Arc<UpgradeWatch>, drain: tokio_util::sync::CancellationToken) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            watch,
            busy: std::sync::atomic::AtomicUsize::new(0),
            parked: tokio_util::sync::CancellationToken::new(),
            pending_since: std::sync::Mutex::new(None),
            drain,
        })
    }
    /// Run `future` (one tick) with these safe points. Only the tick's own
    /// task sees them: spawned tasks and other lanes never park.
    pub async fn scope<F: std::future::Future>(self: &std::sync::Arc<Self>, future: F) -> F::Output {
        SAFE_POINTS.scope(self.clone(), future).await
    }
    /// Whether a deploy is waiting. Also starts the drain once it has waited
    /// past the bound. A watch error is not a deploy.
    pub fn restart_pending(&self) -> bool {
        let changed = self.watch.changed().unwrap_or_else(|error| {
            tracing::warn!("deploy watch unavailable at a safe point; not treated as a deploy: {error:#}");
            false
        });
        if !changed {
            return false;
        }
        let since = *self
            .pending_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert_with(tokio::time::Instant::now);
        if since.elapsed() >= STALE_DEPLOY_BOUND && !self.drain.is_cancelled() {
            tracing::info!(pending_secs = since.elapsed().as_secs(),
                "deploy pending past the stale bound; no new work is claimed until the restart");
            self.drain.cancel();
        }
        true
    }
    /// Resolves once the tick has parked at a safe point.
    pub async fn parked(&self) {
        self.parked.cancelled().await
    }
}

/// A restart-safe point inside a tick (#2354). Returns at once outside a
/// worker tick, with no deploy pending, or while a build or verifier of this
/// tick is in flight. Otherwise it never returns: the worker drops the tick
/// here and restarts onto the deploy.
pub(crate) async fn safe_point() {
    let Ok(points) = SAFE_POINTS.try_with(std::sync::Arc::clone) else {
        return;
    };
    if points.busy.load(std::sync::atomic::Ordering::SeqCst) > 0 || !points.restart_pending() {
        return;
    }
    tracing::info!("deploy pending: tick parked at a safe point; restarting onto the new build");
    points.parked.cancel();
    std::future::pending::<()>().await
}

/// Marks a build or verifier in flight for the current tick; no safe point
/// parks the tick until it is dropped.
pub(crate) struct InFlight(std::sync::Arc<SafePoints>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
pub(crate) fn in_flight() -> Option<InFlight> {
    SAFE_POINTS
        .try_with(|points| {
            points.busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            InFlight(points.clone())
        })
        .ok()
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

/// `AUGMENTAGENT_AUTOPR_EXTERNAL` used to choose between an in-daemon loop and
/// this worker. Auto-ship now runs only here (#2275), so any value is ignored;
/// returns the warning to log when it is still set.
pub fn deprecated_external_flag(value: Option<&str>) -> Option<String> {
    value.map(|value| format!(
        "AUGMENTAGENT_AUTOPR_EXTERNAL={value:?} is deprecated and ignored: auto-ship always runs \
         in the `augmentagent auto-pr-worker` service, never in the daemon; remove it from .env and unit files"
    ))
}

/// Logs the deprecation once at startup when the old mode flag is still set.
pub fn warn_if_external_flag_set() {
    let value = std::env::var("AUGMENTAGENT_AUTOPR_EXTERNAL").ok();
    if let Some(warning) = deprecated_external_flag(value.as_deref()) {
        tracing::warn!("{warning}");
    }
}

/// The daemon never hosts auto-ship, so its marker always reports the
/// external worker. The updater, status and recovery helpers still verify
/// the live daemon's identity (pid and process birth) from this record.
fn host_marker(pid: u32, birth: String) -> serde_json::Value {
    serde_json::json!({"pid": pid, "birth": birth, "external": true})
}

/// This is written by the actual daemon, not by the installer. The updater
/// must not mistake a not-yet-restarted older daemon, which could still host
/// an embedded loop, for one that has restarted onto this build.
pub fn record_host_mode() -> Result<()> {
    let state = crate::build_storage::state_dir()?;
    std::fs::create_dir_all(&state)?;
    let birth = process_birth(Path::new("/proc/self/stat")).unwrap_or_default();
    let value = host_marker(std::process::id(), birth);
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
    /// #2275 — one execution path: the daemon never runs the auto-ship loop,
    /// whatever the OS or `AUGMENTAGENT_AUTOPR_EXTERNAL` say. The serve command
    /// is the only daemon entry point, so it must not construct the loop.
    #[test]
    fn the_daemon_never_hosts_the_autoship_loop() {
        let main = include_str!("main.rs");
        assert!(!main.contains("AutoPrLoop::from_env"),
            "serve must not start an in-daemon auto-ship loop; it runs only in `auto-pr-worker`");
        assert!(!main.contains("external_enabled"),
            "the daemon must not branch on AUGMENTAGENT_AUTOPR_EXTERNAL");
        // Consumers (status, recovery, updater) read this marker; the daemon
        // always reports that the worker, not the daemon, hosts auto-ship.
        let marker = host_marker(31, "77".into());
        assert_eq!(marker["external"], serde_json::json!(true));
        assert_eq!(marker["pid"], serde_json::json!(31));
        assert_eq!(marker["birth"], serde_json::json!("77"));
    }
    #[test]
    fn the_external_flag_is_accepted_but_ignored_with_a_deprecation_warning() {
        for value in ["1", "true", "0", "false", ""] {
            let warning = deprecated_external_flag(Some(value))
                .unwrap_or_else(|| panic!("AUGMENTAGENT_AUTOPR_EXTERNAL={value:?} must warn"));
            assert!(warning.contains("AUGMENTAGENT_AUTOPR_EXTERNAL"), "{warning}");
            assert!(warning.contains("deprecated") && warning.contains("ignored"), "{warning}");
        }
        assert_eq!(deprecated_external_flag(None), None);
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
    /// #2468: a pinned worker watches the pinned build's stamp. A main
    /// deploy (a new `built-commit`) is not a reason to reload; moving the
    /// pin is.
    #[test]
    fn a_pinned_worker_reloads_on_its_pin_and_ignores_main_deploys() {
        let t=tempfile::tempdir().unwrap();
        let state=t.path().join("state");std::fs::create_dir(&state).unwrap();
        let built=state.join("built-commit");std::fs::write(&built,"aaaa\n").unwrap();
        let pinned=state.join("autoship-pinned-built");std::fs::write(&pinned,"pppp\n").unwrap();
        assert_eq!(deployment_stamp_path(None,Some(state.clone())),Some(built.clone()),"unpinned: main's stamp");
        assert_eq!(deployment_stamp_path(Some("".into()),Some(state.clone())),Some(built.clone()),"an empty value is not a pin");
        let stamp=deployment_stamp_path(Some(pinned.clone().into_os_string()),Some(state.clone())).unwrap();
        assert_eq!(stamp,pinned);
        let binary=t.path().join("worker");std::fs::write(&binary,"pinned executable").unwrap();
        let watch=UpgradeWatch::at(&binary).with_deployment_stamp(&stamp);
        std::fs::write(&built,"bbbb\n").unwrap();
        assert!(!watch.changed().unwrap(),"a main deploy must not reload a pinned worker");
        std::fs::write(&pinned,"qqqq\n").unwrap();
        assert!(watch.changed().unwrap(),"a moved pin is a deploy to load");
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
    fn stamped_points(t:&tempfile::TempDir)->(std::sync::Arc<SafePoints>,PathBuf,tokio_util::sync::CancellationToken) {
        let binary=t.path().join("worker");std::fs::write(&binary,"same executable").unwrap();
        let stamp=t.path().join("built-commit");std::fs::write(&stamp,"aaaa\n").unwrap();
        let drain=tokio_util::sync::CancellationToken::new();
        let watch=std::sync::Arc::new(UpgradeWatch::at(&binary).with_deployment_stamp(&stamp));
        (SafePoints::new(watch,drain.clone()),stamp,drain)
    }
    /// #2354: a safe point parks the tick once a deploy is pending, but never
    /// while one of its builds or verifiers is still in flight.
    #[tokio::test(start_paused = true)]
    async fn a_safe_point_parks_after_a_deploy_but_never_inside_a_build() {
        let t=tempfile::tempdir().unwrap();
        let (points,stamp,_)=stamped_points(&t);
        let reached=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen=reached.clone();
        let tick=points.scope(async move {
            safe_point().await;
            seen.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            std::fs::write(&stamp,"bbbb\n").unwrap();
            {
                let _build=in_flight().expect("a tick tracks its builds");
                safe_point().await;
                seen.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            }
            safe_point().await;
            seen.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
        });
        let parked=tokio::time::timeout(std::time::Duration::from_secs(3600),tick).await;
        assert!(parked.is_err(),"the safe point after the build must park the tick");
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst),2,"the build ran to completion; the step after it never started");
        tokio::time::timeout(std::time::Duration::from_secs(1),points.parked()).await
            .expect("the worker is told the tick parked");
    }
    /// Outside a worker tick (CLI work commands, recovery lanes, tests) a
    /// safe point is never a reason to stop.
    #[tokio::test(start_paused = true)]
    async fn a_safe_point_outside_a_tick_never_parks() {
        assert!(in_flight().is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1),safe_point()).await.expect("no tick, no parking");
    }
    /// #2354: a deploy pending past the bound starts the drain, so concurrent
    /// lanes stop claiming new work while the tick finishes its step.
    #[tokio::test(start_paused = true)]
    async fn a_deploy_pending_past_the_bound_starts_the_drain() {
        let t=tempfile::tempdir().unwrap();
        let (points,stamp,drain)=stamped_points(&t);
        assert!(!points.restart_pending());
        std::fs::write(&stamp,"bbbb\n").unwrap();
        assert!(points.restart_pending());
        tokio::time::advance(STALE_DEPLOY_BOUND-std::time::Duration::from_secs(60)).await;
        assert!(points.restart_pending());
        assert!(!drain.is_cancelled(),"a fresh deploy waits for the next safe point");
        tokio::time::advance(std::time::Duration::from_secs(120)).await;
        assert!(points.restart_pending());
        assert!(drain.is_cancelled(),"a deploy pending past the bound drains the worker");
    }
}
