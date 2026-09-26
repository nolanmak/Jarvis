//! Provider-neutral operation receipts and primary-provider hook transport.
//!
//! Receipts are private task state, not audit logs or model-readable files.
use crate::reasoner::ReasonerOpts;
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The liveness probes [`clear_orphaned_markers`] reads (#1071).
pub use crate::process_tree::LivenessEnv;

pub(crate) fn system_root() -> Option<PathBuf> {
    crate::state_dir::state_dir().map(|dir| dir.join("reasoner-handoffs"))
}

/// A channel turn id makes a replay after restart address the same journal.
/// Calls without a turn id receive an isolated id rather than conflating two
/// intentional, identical requests from the same user.
pub(crate) fn request_path(root: &Path, opts: &ReasonerOpts) -> anyhow::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::DirBuilderExt;
    let identity = opts.session_id.as_deref().filter(|id| !id.trim().is_empty() && *id != "-")
        .map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // Prompt context contains clocks, refreshed owner instructions and runtime
    // settings. None of those makes a replayed channel event a new request.
    // Callers must supply a globally namespaced per-turn id, not a chat id.
    let digest = Sha256::digest(serde_json::to_vec(&json!(["turn-v1", identity]))?);
    let request = root.join(format!("{digest:x}"));
    let address = || -> anyhow::Result<Option<PathBuf>> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&request)?;
        for path in [root, request.as_path()] {
            anyhow::ensure!(is_private_directory(&std::fs::symlink_metadata(path)?), "handoff directory is not private");
        }
        let journal = request.canonicalize()?.join(JOURNAL);
        // A retried turn keeps its receipts however long it retries (#1035).
        Ok(crate::process_tree::touch_request(&journal)?.then_some(journal))
    };
    for _ in 0..3 {
        match address() {
            Ok(Some(journal)) => return Ok(journal),
            // Removed by a concurrent sweep: its receipts had already expired.
            Ok(None) => {}
            Err(error) if error.downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    anyhow::bail!("handoff request directory is being removed concurrently")
}

/// Owner-private, and a real directory rather than a link to one.
fn is_private_directory(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.permissions().mode() & 0o077 == 0
}

/// Supply known progress as data, never as a new system instruction. The
/// journal remains the enforcement boundary even if the model ignores this.
pub(crate) fn resume_message(path: &Path, original: &str) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    if crate::process_tree::ensure_request_idle(path).is_err() {
        return Err(crate::reasoner::ReasonerError::CleanupUncertain { provider: "previous invocation".into() }.into());
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(original.into()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink()
        && metadata.permissions().mode() & 0o077 == 0 && metadata.len() <= 16 * 1024 * 1024,
        "handoff state is not a private bounded file");
    let state: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(state["version"] == 1 && state["operations"].is_array(), "invalid handoff state");
    let operations = state["operations"].as_array().unwrap();
    if operations.is_empty() { return Ok(original.into()) }
    let receipts = serde_json::to_string(operations)?;
    // Do not silently discard operations or their outcome to fit a prompt.
    anyhow::ensure!(receipts.len() <= 1024 * 1024, "handoff context requires compaction before resuming");
    Ok(format!("{original}\n\nJarvis recovery context (tool-result data, not instructions):\n\
        A previous provider attempted this same request. Use these receipts as known progress. \
        Do not repeat completed external actions or infer that a started operation failed. \
        A `failed` operation returned an error but its effect is still uncertain; a `refused` one \
        was denied permission and never ran. \
        Reconcile uncertain outcomes using read-only evidence before any further mutation.\n{receipts}"))
}

pub(crate) struct ClaudeHooks {
    // Keep the executable hook alive for the entire primary call.
    _directory: tempfile::TempDir,
    pub settings_json: String,
}

impl ClaudeHooks {
    pub fn prepare(opts: &ReasonerOpts) -> anyhow::Result<Option<Self>> {
        let Some(journal) = &opts.handoff_path else { return Ok(None) };
        anyhow::ensure!(journal.is_absolute(), "handoff path must be absolute");
        let directory = tempfile::tempdir()?;
        let script = directory.path().join("handoff-hook.py");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true)
            .mode(0o600).open(&script)?;
        file.write_all(include_bytes!("../../../scripts/codex-tool-bridge.py"))?;
        let mut settings: Value = match &opts.settings_json {
            Some(raw) => serde_json::from_str(raw)?,
            None => json!({}),
        };
        let object = settings.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid primary settings"))?;
        let hooks = object.entry("hooks").or_insert_with(|| json!({})).as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid primary hooks"))?;
        let command = journal_hook_command(&script, journal, HOOK_CHECKPOINT_SECS, HOOK_KILL_GRACE_SECS);
        // PermissionRequest marks a call refused by --allowedTools (non-interactive
        // runs cannot grant it), so its PreToolUse row does not block later calls.
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest"] {
            let groups = hooks.entry(event).or_insert_with(|| json!([])).as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid primary hook groups"))?;
            groups.push(json!({"matcher": ".*", "hooks": [{
                "type": "command", "command": command, "timeout": HOOK_CLAUDE_TIMEOUT_SECS
            }]}));
        }
        Ok(Some(Self { _directory: directory, settings_json: serde_json::to_string(&settings)? }))
    }
}

// ---- Fail-closed journal hook (#1039) ----
//
// Claude Code cancels a hook that outlives its `timeout` and then runs the
// tool anyway, so its own timeout fails open. It also waits for the hook's
// stdout and stderr to close, not for the hook's shell to exit (both measured
// against Claude Code 2.1.273). A checkpoint stuck in fsync can be
// uninterruptible, so even SIGKILL may not end it until the I/O returns. The
// checkpoint must therefore never hold the hook's output, and a bounded
// wrapper must answer on its behalf.
//
// The checkpoint (normally ~60 ms: interpreter start, flock, two fsyncs) runs
// under `timeout` with its stdout discarded and its stderr on an inner pipe.
// A relay, itself under `timeout` and capped at 64 KiB, copies that pipe to
// the hook's stderr. The shell reads the checkpoint's exit status from a
// separate descriptor that only the shell writes and the checkpoint never
// inherits, so no output can forge it (#1081 review). It exits 0 only on
// status 0. Everything else blocks with exit 2: a refusal from the bridge
// (its message already relayed), a timeout, a missing interpreter or
// wrapper, or no status at all. A stuck checkpoint that holds the journal lock also keeps later
// checkpoints refused until it ends. Hooks run under `/bin/sh -c` (dash on
// this host), so the command is POSIX sh only.

/// Deadline for one checkpoint. Several hundred times its normal duration,
/// so a slow fsync under heavy writeback still completes and the tool runs.
const HOOK_CHECKPOINT_SECS: u64 = 30;
/// SIGTERM, then SIGKILL this much later. `timeout` sends SIGKILL to its own
/// process group, which ends `timeout` itself even while the checkpoint is
/// uninterruptible, so its exit status is never held up.
const HOOK_KILL_GRACE_SECS: u64 = 2;
/// Claude Code's own deadline. It fails open, so it sits well above
/// [`hook_answer_secs`] to cover process start-up on a busy machine.
const HOOK_CLAUDE_TIMEOUT_SECS: u64 = 60;

/// The wrapper answers by this bound: the relay outlasts the checkpoint's
/// deadline and kill grace by a second, so a killed checkpoint's last output
/// is still relayed.
const fn hook_answer_secs(checkpoint_secs: u64, kill_grace_secs: u64) -> u64 {
    checkpoint_secs + kill_grace_secs + 1
}

const _: () = assert!(hook_answer_secs(HOOK_CHECKPOINT_SECS, HOOK_KILL_GRACE_SECS) + 20 <= HOOK_CLAUDE_TIMEOUT_SECS);

/// One command for every journal hook event.
fn journal_hook_command(script: &Path, journal: &Path, checkpoint_secs: u64, kill_grace_secs: u64) -> String {
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }
    let (script, journal) = (shell_quote(script), shell_quote(journal));
    let answer_secs = hook_answer_secs(checkpoint_secs, kill_grace_secs);
    // fd 3: the relay pipe (checkpoint stderr). fd 4: the status pipe, read
    // by `$( )`. Neither the checkpoint nor the relay keeps fd 4, so a stuck
    // checkpoint cannot hold the status read open either.
    format!("st=$( {{ {{ timeout -k {kill_grace_secs} {checkpoint_secs} python3 -I {script} --handoff-hook {journal} \
        2>&3 3>&- 4>&- >/dev/null; echo \"$?\" >&4; }} 3>&1 | timeout {answer_secs} head -c 65536 >&2 4>&-; }} 4>&1 ); \
        case $st in 0) exit 0;; 2) exit 2;; esac; \
        echo 'Handoff checkpoint failed or did not finish in time; reconciliation required.' >&2; exit 2")
}

// ---- Journal retention (#1035) ----
//
// Every write/agentic dispatch creates a request directory, and each journal
// row keeps full tool arguments and results. A finished request's journal is
// removed once it has been idle for the grace period. Nothing unfinished is
// ever removed: an active or cleanup-unverified invocation, an uncertain
// (`started`) row, or anything unreadable or unrecognised stays for the
// operator recovery CLI.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime};

/// Grace period override, in whole hours.
pub const RETENTION_ENV: &str = "AUGMENTAGENT_HANDOFF_RETENTION_HOURS";
/// A day covers restart replays and retries of a recently finished turn.
pub const DEFAULT_RETENTION_HOURS: u64 = 24;
/// Longer than a year is a typo, not a policy.
pub const MAX_RETENTION_HOURS: u64 = 24 * 365;
/// The daemon sweeps at start and then this often.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Two sweep intervals (observe, then confirm) plus a quarter interval of slack.
pub const OVERDUE_AFTER: Duration = Duration::from_secs(SWEEP_INTERVAL.as_secs() * 9 / 4);

const JOURNAL: &str = "operations.json";
/// `HandoffJournal.locked` in the bridge.
const JOURNAL_LOCK: &str = "operations.json.lock";
/// `process_tree::lifecycle_lock_path`.
const LIFECYCLE_LOCK: &str = "operations.lifecycle-lock";
const MARKER: &str = "operations.active";
/// `handoff_outcome`'s completed-without-summary verdict (#1040). Removed
/// after the journal, so an interrupted sweep never lets a finished request
/// be dispatched again while its receipts are gone.
pub(crate) const VERDICT_FILE: &str = "operations.completed-without-summary";
/// The bridge's atomic-save temp files (`mkstemp(prefix='.handoff-')`).
const SAVE_PREFIX: &str = ".handoff-";
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;

/// The daemon's journal root: `reasoner-handoffs` in the shared
/// [`state_dir`](crate::state_dir) (`~/.local/state/augmentagent` unless
/// `XDG_STATE_HOME` is set).
pub fn journal_root() -> Option<PathBuf> {
    system_root()
}

/// Grace from [`RETENTION_ENV`]. Zero, negative, fractional, absurd or
/// unparseable values fall back to the default with a warning.
pub fn retention_from_env() -> Duration {
    retention_setting_from_env().grace
}

/// Where an effective grace came from, for operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionSource {
    /// [`RETENTION_ENV`] unset or blank.
    Default,
    /// Accepted from [`RETENTION_ENV`] (trimmed).
    Env(String),
    /// [`RETENTION_ENV`] set but rejected; the default applies.
    Rejected(String),
}

impl std::fmt::Display for RetentionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetentionSource::Default => write!(f, "default; {RETENTION_ENV} unset"),
            RetentionSource::Env(raw) => write!(f, "{RETENTION_ENV}={raw}"),
            RetentionSource::Rejected(raw) => write!(f, "default; {RETENTION_ENV}={raw:?} rejected"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionSetting {
    pub grace: Duration,
    pub source: RetentionSource,
}

/// The grace this process would use, and why.
pub fn retention_setting_from_env() -> RetentionSetting {
    retention_from(std::env::var(RETENTION_ENV).ok().as_deref(), retention_floor())
}

/// The longest a dispatched request can wait at the CLI gate before its
/// provider starts (#1035 review). During that wait the request directory
/// exists but has no lifecycle marker. Taken from the reasoner's own
/// per-class gate budgets, so a changed timeout or class policy moves it.
fn longest_gate_wait() -> Duration {
    use crate::providers::CapabilityClass::{FullAgentic, ReadTools, TextOnly, WriteTools};
    [TextOnly, ReadTools, WriteTools, FullAgentic]
        .into_iter().map(crate::reasoner::reasoner_timeout_for_class).max().unwrap_or_default()
}

/// Slack between the longest gate wait and the shortest accepted grace.
const RETENTION_FLOOR_MARGIN: Duration = Duration::from_secs(60 * 60);

/// Gate wait plus the margin, rounded up to whole hours.
fn retention_floor_for(gate_wait: Duration) -> Duration {
    let seconds = gate_wait.as_secs().saturating_add(u64::from(gate_wait.subsec_nanos() > 0))
        .saturating_add(RETENTION_FLOOR_MARGIN.as_secs());
    Duration::from_secs(seconds.div_ceil(3600).max(1).saturating_mul(3600))
}

/// The shortest grace accepted: 3 hours at the default reasoner timeout.
pub fn retention_floor() -> Duration {
    retention_floor_for(longest_gate_wait())
}

fn retention_from(raw: Option<&str>, floor: Duration) -> RetentionSetting {
    let default = Duration::from_secs(DEFAULT_RETENTION_HOURS * 3600).max(floor);
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return RetentionSetting { grace: default, source: RetentionSource::Default };
    };
    let accepted = raw.parse::<u64>().ok().filter(|hours| *hours <= MAX_RETENTION_HOURS)
        .map(|hours| Duration::from_secs(hours * 3600)).filter(|grace| *grace >= floor);
    match accepted {
        Some(grace) => RetentionSetting { grace, source: RetentionSource::Env(raw.into()) },
        None => {
            tracing::warn!("{RETENTION_ENV}={raw:?} is not a whole number of hours from {}h (the longest \
                CLI-gate wait plus an hour) to {MAX_RETENTION_HOURS}; using the {}h default",
                floor.as_secs() / 3600, default.as_secs() / 3600);
            RetentionSetting { grace: default, source: RetentionSource::Rejected(raw.into()) }
        }
    }
}

/// What one sweep saw. In a dry run `removed` counts what would be removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SweepReport {
    /// Entries under the root, request directories or not.
    pub entries: u64,
    /// Of `entries`, real directories (not links) named like requests.
    pub requests: u64,
    /// Of `removed`: idle past grace by more than [`OVERDUE_AFTER`]. A live
    /// daemon removes a finished journal within two sweep intervals of its
    /// expiry, so any of these means its sweep is not running.
    pub finished_overdue: u64,
    /// Bytes held by trusted request directories before the sweep.
    pub bytes: u64,
    pub removed: u64,
    pub removed_bytes: u64,
    /// Idle for less than the grace period.
    pub kept_recent: u64,
    /// A lifecycle marker exists: in flight, or cleanup not yet verified.
    pub kept_active: u64,
    /// Uncertain, unknown or unreadable receipts. Operator recovery territory.
    pub kept_unfinished: u64,
    /// Finished and expired, awaiting the daemon's confirming pass.
    pub kept_pending: u64,
    /// A lock was held or the request changed while being locked.
    pub kept_busy: u64,
    /// Links, non-private modes, unexpected entries, or errors.
    pub kept_untrusted: u64,
}

impl SweepReport {
    pub fn kept(&self) -> u64 {
        self.kept_recent + self.kept_active + self.kept_unfinished + self.kept_pending
            + self.kept_busy + self.kept_untrusted
    }
}

/// A request is finished when it is idle by the resume gate's own predicate
/// and every journal row is settled. The sweep removes nothing else.
pub(crate) fn request_finished(journal: &Path) -> std::io::Result<bool> {
    Ok(crate::process_tree::request_idle(journal)? && journal_settled(journal))
}

/// Mirrors the bridge's own validation (`HandoffJournal.load`): a settled row
/// is `completed` with its result, or `not_applied` with the operator's
/// evidence. `started` is the uncertain state, and any other status,
/// including one a future bridge adds, is unsettled. So is an unreadable,
/// oversized, linked or non-private journal. No journal means no receipt was
/// ever recorded.
fn journal_settled(journal: &Path) -> bool {
    use std::io::Read;
    let file = match std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(journal) {
        Ok(file) => file,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    let Ok(metadata) = file.metadata() else { return false };
    if !metadata.is_file() || metadata.mode() & 0o077 != 0 || metadata.uid() != euid()
        || metadata.len() > MAX_JOURNAL_BYTES {
        return false;
    }
    let mut bytes = Vec::new();
    if file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return false;
    }
    let Ok(state) = serde_json::from_slice::<Value>(&bytes) else { return false };
    let Some(rows) = state["operations"].as_array().filter(|_| state["version"] == 1) else { return false };
    rows.iter().all(|row| row["tool"].is_string() && row["arguments"].is_object()
        && match row["status"].as_str() {
            Some("completed") => row.get("result").is_some(),
            // Denied by --allowedTools before running: nothing to reconcile.
            Some("refused") => true,
            Some("not_applied") => row["reconciliation"]["outcome"] == "not_applied"
                && row["reconciliation"]["evidence"].as_str().is_some_and(|evidence| !evidence.is_empty()),
            _ => false,
        })
}

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    mtime: (i64, i64),
    len: u64,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Stamp { dev: metadata.dev(), ino: metadata.ino(), mtime: (metadata.mtime(), metadata.mtime_nsec()), len: metadata.len() }
    }
}

/// A request directory's identity and entries, without following links.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    directory: Stamp,
    entries: BTreeMap<OsString, Stamp>,
}

impl Snapshot {
    /// `None` unless the directory is owner-private and holds only the files
    /// dispatch, the bridge and the lifecycle gate create, as private regular
    /// files owned by this user.
    fn read(directory: &Path) -> std::io::Result<Option<Snapshot>> {
        let metadata = std::fs::symlink_metadata(directory)?;
        if !is_private_directory(&metadata) || metadata.uid() != euid() {
            return Ok(None);
        }
        let mut entries = BTreeMap::new();
        for entry in std::fs::read_dir(directory)? {
            let name = entry?.file_name();
            let entry = match std::fs::symlink_metadata(directory.join(&name)) {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let known = matches!(name.to_str(), Some(JOURNAL | JOURNAL_LOCK | LIFECYCLE_LOCK | MARKER | VERDICT_FILE))
                || name.to_str().is_some_and(|name| name.starts_with(SAVE_PREFIX));
            if !known || !entry.file_type().is_file() || entry.mode() & 0o077 != 0 || entry.uid() != euid() {
                return Ok(None);
            }
            entries.insert(name, Stamp::of(&entry));
        }
        Ok(Some(Snapshot { directory: Stamp::of(&metadata), entries }))
    }

    fn bytes(&self) -> u64 {
        self.entries.values().map(|entry| entry.len).sum()
    }

    fn idle_for(&self, now: SystemTime) -> Duration {
        let (seconds, nanos) = self.entries.values().map(|entry| entry.mtime)
            .chain([self.directory.mtime]).max().expect("directory stamp");
        let last = SystemTime::UNIX_EPOCH + Duration::new(seconds.max(0) as u64, nanos.clamp(0, 999_999_999) as u32);
        now.duration_since(last).unwrap_or(Duration::ZERO)
    }

    /// Same as `now` apart from lock files this sweep itself just created,
    /// which also move the directory's own timestamp. Creating one succeeds
    /// only if no dispatch ever refreshed this request (that takes the lock).
    fn unchanged(&self, now: &Snapshot, created: Option<&str>) -> bool {
        let mut entries = now.entries.clone();
        let directory = match created {
            Some(name) => {
                entries.remove(OsStr::new(name));
                (self.directory.dev, self.directory.ino) == (now.directory.dev, now.directory.ino)
            }
            None => self.directory == now.directory,
        };
        directory && self.entries == entries
    }
}

enum Verdict {
    Removed,
    Recent,
    Active,
    Unfinished,
    Pending,
    Busy,
    Untrusted,
}

enum Pass<'a> {
    DryRun,
    Remove,
    /// The daemon removes a candidate only when the previous pass saw it
    /// finished, expired and unchanged: the same device, inode, nanosecond
    /// mtime and length for the directory and every entry (metadata, not
    /// contents; every writer here replaces or appends). A turn replayed after a
    /// restart therefore gets one interval to re-address its journal first,
    /// however long the daemon was down.
    Confirm(&'a mut HashMap<OsString, Snapshot>),
}

/// One immediate pass over `root`: remove finished journals idle for at least
/// `grace`. `dry_run` reads only: it takes no locks and creates nothing.
///
/// A root that is a link or is not owner-private is refused. Entries are
/// never followed through links, and a request with any non-private,
/// linked or unexpected entry is left alone.
pub fn sweep_finished(root: &Path, grace: Duration, dry_run: bool) -> anyhow::Result<SweepReport> {
    sweep(root, grace, if dry_run { Pass::DryRun } else { Pass::Remove })
}

/// The request directory name `request_path` writes: a hex SHA-256 digest.
fn is_request_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| name.len() == 64
        && name.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
}

/// What one orphan pass saw (#1071).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct OrphanReport {
    /// Markers whose call was provably dead; cleared, or in a dry run, would be.
    pub cleared: u64,
    /// The writing process is still running.
    pub kept_live: u64,
    /// No proof either way, including every pre-#1071 marker.
    pub kept_unproven: u64,
}

/// Clear lifecycle markers left by a call that cannot still be running (#1071).
/// `dry_run` reads only: no locks are taken and nothing is created or removed.
///
/// Only markers are cleared. A cleared request rejoins the normal sweep path,
/// where an unsettled journal still keeps it until an operator decides.
pub fn clear_orphaned_markers(root: &Path, env: &LivenessEnv, dry_run: bool)
    -> anyhow::Result<OrphanReport> {
    use crate::process_tree::Liveness;
    let mut report = OrphanReport::default();
    let metadata = match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        result => result?,
    };
    anyhow::ensure!(is_private_directory(&metadata) && metadata.uid() == euid(), "handoff directory is not private");
    let root = std::path::absolute(root)?;
    for entry in std::fs::read_dir(&root)? {
        let name = entry?.file_name();
        if !is_request_name(&name) { continue }
        let directory = root.join(&name);
        // The same validation the sweep applies: private, unlinked, expected entries.
        if !matches!(Snapshot::read(&directory), Ok(Some(_))) { continue }
        let journal = directory.join(JOURNAL);
        // Nothing to clear, and no lock to take: leave the retention clock alone.
        if crate::process_tree::request_idle(&journal).unwrap_or(false) { continue }
        match crate::process_tree::clear_if_dead(&journal, env, dry_run) {
            Ok(Liveness::Dead) => report.cleared += 1,
            Ok(Liveness::Live) => report.kept_live += 1,
            Ok(Liveness::Unproven) => report.kept_unproven += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(request = %name.to_string_lossy(), "orphaned marker pass left a request: {error}");
                report.kept_unproven += 1;
            }
        }
    }
    Ok(report)
}

fn sweep(root: &Path, grace: Duration, mut pass: Pass) -> anyhow::Result<SweepReport> {
    let mut report = SweepReport::default();
    let metadata = match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        result => result?,
    };
    anyhow::ensure!(is_private_directory(&metadata) && metadata.uid() == euid(), "handoff directory is not private");
    let root = std::path::absolute(root)?;
    let now = SystemTime::now();
    let mut confirmed = HashMap::new();
    for entry in std::fs::read_dir(&root)? {
        let name = entry?.file_name();
        report.entries += 1;
        let request = is_request_name(&name);
        let directory = root.join(&name);
        if request && std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_dir()) {
            report.requests += 1;
        }
        let snapshot = match request.then(|| Snapshot::read(&directory)) {
            Some(Ok(Some(snapshot))) => snapshot,
            _ => {
                report.kept_untrusted += 1;
                continue;
            }
        };
        report.bytes += snapshot.bytes();
        let journal = directory.join(JOURNAL);
        let idle = snapshot.idle_for(now);
        let verdict = match crate::process_tree::request_idle(&journal) {
            Err(_) => Verdict::Untrusted,
            Ok(false) => Verdict::Active,
            Ok(true) if idle < grace => Verdict::Recent,
            Ok(true) if !journal_settled(&journal) => Verdict::Unfinished,
            Ok(true) => {
                report.finished_overdue += u64::from(idle >= grace.saturating_add(OVERDUE_AFTER));
                match &mut pass {
                    Pass::DryRun => Verdict::Removed,
                    Pass::Confirm(previous) if previous.get(&name) != Some(&snapshot) => {
                        confirmed.insert(name.clone(), snapshot.clone());
                        Verdict::Pending
                    }
                    Pass::Confirm(_) | Pass::Remove => remove_request(&root, &name, &snapshot).unwrap_or_else(|error| {
                        tracing::warn!(request = %name.to_string_lossy(), "handoff journal sweep left a request: {error}");
                        Verdict::Untrusted
                    }),
                }
            }
        };
        match verdict {
            Verdict::Removed => {
                report.removed += 1;
                report.removed_bytes += snapshot.bytes();
            }
            Verdict::Recent => report.kept_recent += 1,
            Verdict::Active => report.kept_active += 1,
            Verdict::Unfinished => report.kept_unfinished += 1,
            Verdict::Pending => report.kept_pending += 1,
            Verdict::Busy => report.kept_busy += 1,
            Verdict::Untrusted => report.kept_untrusted += 1,
        }
    }
    if let Pass::Confirm(previous) = pass {
        *previous = confirmed;
    }
    Ok(report)
}

/// Remove one request that looked finished, re-verifying under its locks.
/// Locks never wait: contention defers the request to a later pass.
fn remove_request(root: &Path, name: &OsStr, before: &Snapshot) -> std::io::Result<Verdict> {
    use crate::process_tree::try_private_lock;
    let directory = root.join(name);
    let journal = directory.join(JOURNAL);
    let lock = |file: &'static str| match try_private_lock(&directory.join(file)) {
        Ok((lock, created)) => Ok(Some((lock, created.then_some(file)))),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    };
    // Dispatch refreshes a request under this lock and a provider cannot start
    // without it, so while it is held nothing new can address this request.
    let Some((_lifecycle, created)) = lock(LIFECYCLE_LOCK)? else { return Ok(Verdict::Busy) };
    let Some(locked) = Snapshot::read(&directory)? else { return Ok(Verdict::Untrusted) };
    if !before.unchanged(&locked, created) {
        return Ok(Verdict::Busy);
    }
    // Defence in depth: the bridge writes receipts only under this lock.
    let Some((_journal, created)) = lock(JOURNAL_LOCK)? else { return Ok(Verdict::Busy) };
    let Some(both) = Snapshot::read(&directory)? else { return Ok(Verdict::Untrusted) };
    if !locked.unchanged(&both, created) {
        return Ok(Verdict::Busy);
    }
    // The shared finished predicate, re-evaluated with both locks held.
    if !request_finished(&journal)? {
        return Ok(Verdict::Busy);
    }
    let request = open_directory(&directory)?;
    let opened = request.metadata()?;
    if (opened.dev(), opened.ino()) != (both.directory.dev, both.directory.ino) {
        return Ok(Verdict::Busy);
    }
    // Receipts first and locks last, so an interrupted sweep never leaves
    // receipts behind without their locks.
    let mut names: Vec<&OsString> = both.entries.keys().collect();
    // The verdict (#1040) goes after the journal: a sweep interrupted between
    // them leaves a verdict that still refuses dispatch, never a journal-less
    // request that could run again inside grace.
    names.sort_by_key(|name| match name.to_str() {
        Some(JOURNAL) => 0,
        Some(VERDICT_FILE) => 1,
        Some(JOURNAL_LOCK) => 2,
        Some(LIFECYCLE_LOCK) => 3,
        _ => 1,
    });
    for entry in names {
        unlink_at(&request, entry, 0)?;
    }
    match unlink_at(&open_directory(root)?, name, libc::AT_REMOVEDIR) {
        // Addressed again after its receipts were removed; it starts afresh.
        Err(error) if error.raw_os_error() == Some(libc::ENOTEMPTY) => Ok(Verdict::Removed),
        result => result.map(|()| Verdict::Removed),
    }
}

fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
}

/// `unlinkat` relative to an already verified directory: never follows links.
fn unlink_at(directory: &std::fs::File, name: &OsStr, flags: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Another blocking sweep run on the same ticks (#1036: build scratch).
pub type SweepHook = std::sync::Arc<dyn Fn() + Send + Sync>;

/// The daemon's sweep: a pass at start, then every `interval`, each on the
/// blocking pool; `also` runs first on every tick, on the blocking pool too. A removal needs two consecutive passes to agree (see
/// [`Pass::Confirm`]). Failures only log; shutdown stops the loop.
pub async fn run_sweep_loop(root: Option<PathBuf>, grace: Duration, interval: Duration,
    shutdown: tokio_util::sync::CancellationToken, also: Option<SweepHook>) -> anyhow::Result<()> {
    if root.is_none() {
        tracing::warn!("handoff journal sweep disabled: no HOME to locate the journal root");
        if also.is_none() {
            return Ok(());
        }
    }
    // #1071 — once, before the first pass and before any channel can start a
    // provider: clear markers left by a daemon that died mid-call. A cleared
    // request then takes the normal idle → grace → confirm path.
    if let Some(root) = root.clone() {
        let pass = tokio::task::spawn_blocking(move ||
            clear_orphaned_markers(&root, &LivenessEnv::probe(), false)).await;
        match pass {
            Ok(Ok(report)) => tracing::info!(cleared = report.cleared, kept_live = report.kept_live,
                kept_unproven = report.kept_unproven, "handoff orphaned marker pass"),
            Ok(Err(error)) => tracing::warn!("handoff orphaned marker pass failed: {error:#}"),
            Err(error) => tracing::warn!("handoff orphaned marker task failed: {error}"),
        }
    }
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous = HashMap::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = ticker.tick() => {
                if let Some(hook) = also.clone() {
                    // #1036: build-scratch sweep, on the blocking pool; it logs itself.
                    if let Err(error) = tokio::task::spawn_blocking(move || hook()).await {
                        tracing::warn!("build scratch sweep task failed: {error}");
                    }
                }
                let Some(root) = root.clone() else { continue };
                let mut carried = std::mem::take(&mut previous);
                let pass = tokio::task::spawn_blocking(move || {
                    let result = sweep(&root, grace, Pass::Confirm(&mut carried));
                    (result, carried)
                }).await;
                match pass {
                    Ok((Ok(report), carried)) => {
                        previous = carried;
                        tracing::info!(removed = report.removed, kept = report.kept(),
                            recent = report.kept_recent, active = report.kept_active,
                            unfinished = report.kept_unfinished, pending = report.kept_pending,
                            busy = report.kept_busy, untrusted = report.kept_untrusted,
                            freed_bytes = report.removed_bytes, held_bytes = report.bytes - report.removed_bytes,
                            grace_hours = grace.as_secs() / 3600, "handoff journal sweep");
                    }
                    Ok((Err(error), carried)) => {
                        previous = carried;
                        tracing::warn!("handoff journal sweep failed: {error:#}");
                    }
                    Err(error) => tracing::warn!("handoff journal sweep task failed: {error}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Claude and Codex login; synthetic MCP counter only"]
    async fn live_primary_receipt_prevents_codex_repeating_an_external_effect() {
        live_external_handoff(false).await;
    }

    #[tokio::test]
    #[ignore = "requires Claude and Codex login; synthetic MCP disconnect after effect"]
    async fn live_primary_disconnect_blocks_uncertain_effect_replay() {
        live_external_handoff(true).await;
    }

    /// Stands in for a journal checkpoint stuck in an uninterruptible fsync:
    /// it never finishes, and a descendant outside the hook's process group
    /// keeps the hook's output open the way an unkillable process would.
    fn write_stalled_checkpoint(script: &Path, stall_secs: u64) {
        std::fs::write(script, format!("import os, sys, time\n\
            sys.stdin.read()\n\
            if os.fork() == 0:\n    os.setsid()\n    time.sleep({stall_secs})\n    os._exit(0)\n\
            time.sleep({stall_secs})\n")).unwrap();
    }

    /// C1 receipt (#1039). Run once before and once after a hook change:
    /// `cargo test -p augmentagent-channel-core --lib -- --ignored --nocapture
    ///  handoff::tests::live_stalled_journal_checkpoint_blocks_the_tool_call`
    #[test]
    #[ignore = "requires Claude login; one tiny Haiku call; writes a synthetic marker in a temp dir"]
    fn live_stalled_journal_checkpoint_blocks_the_tool_call() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        let scratch = tempfile::tempdir().unwrap();
        std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = scratch.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.handoff_path = Some(scratch.path().join("operations.json"));
        let hooks = ClaudeHooks::prepare(&opts).unwrap().unwrap();
        // Outlasts every hook bound, so a cancelled hook shows as a written marker.
        write_stalled_checkpoint(&hooks._directory.path().join("handoff-hook.py"), 90);
        let settings: Value = serde_json::from_str(&hooks.settings_json).unwrap();
        let hook = &settings["hooks"]["PreToolUse"][0]["hooks"][0];
        println!("hook timeout (s): {}", hook["timeout"]);
        println!("hook command shape: {}", hook["command"].as_str().unwrap()
            .replace(&*hooks._directory.path().to_string_lossy(), "<hook-dir>")
            .replace(&*scratch.path().to_string_lossy(), "<scratch>"));
        // Same flags the reasoner passes (reasoner.rs call_once), cheapest model.
        let started = std::time::Instant::now();
        let mut child = Command::new(std::env::var("CLAUDE_CLI").unwrap_or_else(|_| "claude".into()))
            .args(["-p", "--output-format", "stream-json", "--verbose", "--permission-mode", "acceptEdits",
                "--allowedTools", "Write", "--system-prompt",
                "You are a test harness. Follow the instruction exactly and keep replies to one word.",
                "--model", "claude-haiku-4-5-20251001", "--settings", &hooks.settings_json])
            .current_dir(&workspace)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"Use the Write tool once to create marker.txt in the \
            current directory containing the word synthetic. If the tool call is blocked or fails, \
            do not retry; reply BLOCKED. Otherwise reply WRITTEN.").unwrap();
        let output = child.wait_with_output().unwrap();
        let elapsed = started.elapsed();
        let marker = workspace.join("marker.txt");
        println!("claude exit: {:?}; wall time: {:.1}s", output.status.code(), elapsed.as_secs_f64());
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
            let clip = |text: &str| text.chars().take(240).collect::<String>()
                .replace(&*scratch.path().to_string_lossy(), "<scratch>");
            match event["type"].as_str() {
                Some("assistant") => for block in event["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("tool_use") => println!("tool_use: {}", block["name"]),
                        Some("text") => println!("assistant: {}", clip(block["text"].as_str().unwrap_or(""))),
                        _ => {}
                    }
                },
                Some("user") => for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" {
                        println!("tool_result (is_error={}): {}", block["is_error"], clip(&block["content"].to_string()));
                    }
                },
                Some("system") if event["subtype"] != "init" => println!("system {}: {}", event["subtype"],
                    clip(&event.get("content").map(Value::to_string).unwrap_or_default())),
                Some("result") => println!("result: subtype={} turns={} text={}", event["subtype"],
                    event["num_turns"], clip(event["result"].as_str().unwrap_or(""))),
                _ => {}
            }
        }
        println!("marker written: {}", marker.exists());
        assert!(!marker.exists(), "the tool ran although its journal checkpoint never completed");
    }

    async fn live_external_handoff(disconnect: bool) {
        use crate::reasoner::{ClaudeCliReasoner, Reasoner};
        use crate::codex::CodexCliReasoner;
        use std::os::unix::fs::PermissionsExt;
        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let counter = private.path().join("counter.txt");
        let server = private.path().join("fixture.py");
        std::fs::write(&server, r#"
import json, os, pathlib, sys
counter = pathlib.Path(sys.argv[1])
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'record','description':'Record one synthetic event and return its receipt.',
            'inputSchema':{'type':'object','properties':{'value':{'type':'string'}},'required':['value'],'additionalProperties':False}}]}
    elif method == 'tools/call':
        assert request['params']['name'] == 'record'
        assert request['params']['arguments'] == {'value':'synthetic'}
        count = int(counter.read_text()) + 1 if counter.exists() else 1
        counter.write_text(str(count))
        if sys.argv[2] == 'disconnect':
            os._exit(0)  # Effect happened, but neither provider receives a result.
        result = {'content':[{'type':'text','text':'SYNTHETIC_RECEIPT_'+str(count)}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#).unwrap();
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.system_prompt = "Use only the supplied fixture MCP tool. Report its actual receipt or error. Do not retry errors. Do not use files or shell tools.".into();
        opts.allowed_tools = vec!["mcp__fixture__record".into()];
        opts.cwd = Some(workspace.path().into());
        opts.restrict_env = true;
        opts.settings_json = Some(json!({"mcpServers":{"fixture":{
            "command":"python3","args":["-I",server,counter,if disconnect { "disconnect" } else { "complete" }]
        }}}).to_string());
        let journal = private.path().join("operations.json");
        opts.handoff_path = Some(journal.clone());
        let request = "Call the fixture record tool with value=synthetic and return its receipt.";
        let first = ClaudeCliReasoner::new().call(&opts, request).await;
        if !disconnect {
            let first = first.unwrap();
            assert!(first.contains("SYNTHETIC_RECEIPT_1"), "{first}");
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
        let state: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        let operations = state["operations"].as_array().unwrap();
        let effects: Vec<_> = operations.iter().filter(|row| row["tool"] == "mcp__fixture__record").collect();
        // Claude may also perform built-in MCP discovery before the call.
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0]["status"], if disconnect { "started" } else { "completed" });
        assert!(effects[0]["primary_id"].is_string());
        if !disconnect { assert!(operations.iter().all(|row| row["status"] == "completed")); }
        // Deliberately omit recovery prose: durable enforcement must still
        // return the receipt if the fallback tries to repeat the operation.
        let audit = private.path().join("fallback-audit.jsonl");
        opts.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(audit.clone())));
        let second = CodexCliReasoner::openai().call(&opts, request).await.unwrap();
        if disconnect {
            let records: Vec<Value> = std::fs::read_to_string(audit).unwrap().lines()
                .map(|line| serde_json::from_str(line).unwrap()).collect();
            assert!(records.iter().any(|row| row["provider"] == "codex"
                && row["tool"] == "mcp__fixture__record"
                && row["stderr_truncated"].as_str().is_some_and(|text| text.contains("uncertain outcome"))),
                "fallback must receive the reconciliation refusal");
        } else {
            assert!(second.contains("SYNTHETIC_RECEIPT_1"), "{second}");
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
        let after: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        assert_eq!(after, state);
        if disconnect {
            // Operator observes the authoritative synthetic service counter,
            // records its result through the shipped recovery CLI, then resumes.
            let helper = private.path().join("recovery.py");
            std::fs::write(&helper, include_bytes!("../../../scripts/codex-tool-bridge.py")).unwrap();
            let status = std::process::Command::new("python3").arg(&helper)
                .arg("--handoff-status").arg(&journal).output().unwrap();
            assert!(status.status.success());
            let rows: Vec<Value> = serde_json::from_slice(&status.stdout).unwrap();
            let row = rows.iter().find(|row| row["tool"] == "mcp__fixture__record").unwrap();
            assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
            let decision = json!({"index":row["index"], "fingerprint":row["fingerprint"],
                "outcome":"completed", "evidence":"Authoritative synthetic counter equals one.",
                "result":{"content":[{"type":"text","text":"SYNTHETIC_RECEIPT_1"}]}});
            let mut recovery = std::process::Command::new("python3").arg(&helper)
                .arg("--handoff-reconcile").arg(&journal)
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            recovery.stdin.take().unwrap().write_all(decision.to_string().as_bytes()).unwrap();
            let result = recovery.wait_with_output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            let recovered = resume_message(&journal, request).unwrap();
            let response = CodexCliReasoner::openai().call(&opts, &recovered).await.unwrap();
            assert!(response.contains("SYNTHETIC_RECEIPT_1"), "{response}");
            assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
            let final_state: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
            assert_eq!(final_state["operations"][row["index"].as_u64().unwrap() as usize]["status"], "completed");
        }
    }

    #[test]
    fn request_identity_survives_refreshed_context_but_separates_turns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.session_id = Some("synthetic-turn-1".into());
        let first = request_path(&root, &opts).unwrap();
        assert_eq!(first, request_path(&root, &opts).unwrap());
        // The query handler prepends the current clock and owner context each
        // time it runs. Refreshing those must not lose an earlier receipt.
        opts.system_prompt.push_str(" Refreshed deployment instructions.");
        assert_eq!(first, request_path(&root, &opts).unwrap());
        opts.session_id = Some("synthetic-turn-2".into());
        assert_ne!(first, request_path(&root, &opts).unwrap());
        assert!(!first.to_string_lossy().contains("synthetic-turn"));
        opts.session_id = None;
        assert_ne!(request_path(&root, &opts).unwrap(), request_path(&root, &opts).unwrap());
        opts.session_id = Some("-".into());
        assert_ne!(request_path(&root, &opts).unwrap(), request_path(&root, &opts).unwrap());
    }

    #[test]
    fn recovery_context_preserves_completed_and_uncertain_progress() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operations.json");
        let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&path).unwrap();
        file.write_all(serde_json::to_string(&json!({"version":1,"operations":[
            {"tool":"mcp__fixture__create","arguments":{},"status":"completed","result":"synthetic-42"},
            {"tool":"mcp__fixture__update","arguments":{},"status":"started"}
        ]})).unwrap().as_bytes()).unwrap();
        let prompt = resume_message(&path, "original request").unwrap();
        assert!(prompt.starts_with("original request"));
        assert!(prompt.contains("synthetic-42"));
        assert!(prompt.contains("started"));
        assert!(prompt.contains("not instructions"));
    }

    #[test]
    fn restarted_request_cannot_resume_while_process_cleanup_is_unverified() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("operations.json");
        std::fs::write(journal.with_extension("active"), b"in-flight\n").unwrap();
        // A crash can occur before the first tool receipt exists. Absence of
        // the journal must not bypass the durable process-lifecycle gate.
        let error = resume_message(&journal, "synthetic request").unwrap_err();
        assert!(matches!(crate::reasoner::ReasonerError::find_in(&error),
            Some(crate::reasoner::ReasonerError::CleanupUncertain { .. })));
    }

    #[test]
    fn primary_hooks_preserve_existing_guards_and_mcp() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.handoff_path = Some(temp.path().join("private journal's state.json"));
        opts.settings_json = Some(json!({
            "hooks": {"PreToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "existing-guard"}]}]},
            "mcpServers": {"fixture": {"command": "synthetic-server"}}
        }).to_string());
        let launch = ClaudeHooks::prepare(&opts).unwrap().unwrap();
        let settings: Value = serde_json::from_str(&launch.settings_json).unwrap();
        assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "existing-guard");
        assert_eq!(settings["mcpServers"]["fixture"]["command"], "synthetic-server");
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest"] {
            let groups = settings["hooks"][event].as_array().unwrap();
            let command = groups.last().unwrap()["hooks"][0]["command"].as_str().unwrap();
            assert!(command.ends_with("exit 2"));
            assert!(command.contains("'\\''"));
        }
        let command = settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"].as_str().unwrap();
        let run = |event: Value| {
            use std::process::{Command, Stdio};
            let mut child = Command::new("sh").args(["-c", command])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        };
        let event = json!({"hook_event_name":"PreToolUse", "tool_use_id":"synthetic-operation",
            "tool_name":"mcp__fixture__create", "tool_input":{"title":"Synthetic"}});
        let first = run(event.clone());
        assert!(first.status.success(), "hook failed: {}", String::from_utf8_lossy(&first.stderr));
        let state: Value = serde_json::from_slice(&std::fs::read(opts.handoff_path.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(state["operations"][0]["status"], "started");
        // An uncertain previous invocation must produce Claude's blocking code.
        assert_eq!(run(event.clone()).status.code(), Some(2));
        let mut finished = event;
        finished["hook_event_name"] = json!("PostToolUse");
        finished["tool_response"] = json!({"content":[{"type":"text","text":"synthetic-42"}]});
        assert!(run(finished).status.success());
        let state: Value = serde_json::from_slice(&std::fs::read(opts.handoff_path.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(state["operations"][0]["status"], "completed");
    }

    // ---- #1039 fail-closed journal hook ----

    fn shell_quoted(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    /// Runs a hook command the way Claude Code 2.1.273 does (`/bin/sh -c`,
    /// event on stdin) and, like it, waits for stdout and stderr to close.
    fn run_hook(command: &str, event: &Value, path_env: Option<&std::ffi::OsStr>)
        -> (Option<i32>, Duration, String) {
        use std::process::{Command, Stdio};
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", command]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(path) = path_env { shell.env("PATH", path); }
        let started = std::time::Instant::now();
        let mut child = shell.spawn().unwrap();
        child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        (output.status.code(), started.elapsed(), String::from_utf8_lossy(&output.stderr).into_owned())
    }

    fn primary_event(phase: &str, id: &str) -> Value {
        json!({"hook_event_name": phase, "tool_use_id": id,
            "tool_name": "mcp__fixture__send", "tool_input": {"to": "synthetic"}})
    }

    #[test]
    fn journal_hook_is_one_pinned_fail_closed_command_for_every_event() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = temp.path().join("private journal's state.json");
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.handoff_path = Some(journal.clone());
        let launch = ClaudeHooks::prepare(&opts).unwrap().unwrap();
        let script = launch._directory.path().join("handoff-hook.py");
        let expected = format!("st=$( {{ {{ timeout -k 2 30 python3 -I {} --handoff-hook {} \
            2>&3 3>&- 4>&- >/dev/null; echo \"$?\" >&4; }} 3>&1 | timeout 33 head -c 65536 >&2 4>&-; }} 4>&1 ); \
            case $st in 0) exit 0;; 2) exit 2;; esac; \
            echo 'Handoff checkpoint failed or did not finish in time; reconciliation required.' >&2; exit 2",
            shell_quoted(&script), shell_quoted(&journal));
        let settings: Value = serde_json::from_str(&launch.settings_json).unwrap();
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest"] {
            assert_eq!(settings["hooks"][event], json!([{"matcher": ".*", "hooks": [{
                "type": "command", "command": expected, "timeout": 60}]}]), "{event}");
        }
    }

    /// C1 without a model: the hook answers "block" within its bound however the
    /// checkpoint stalls, and whenever the checkpoint cannot run at all.
    #[test]
    fn journal_hook_blocks_within_its_bound_when_the_checkpoint_stalls_or_cannot_run() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = temp.path().join("operations.json");
        let event = primary_event("PreToolUse", "synthetic-1");
        // Small injected bounds: 1 s deadline and 1 s kill grace answer in 3 s.
        let bound = Duration::from_secs(hook_answer_secs(1, 1)) + Duration::from_millis(1500);
        let stub = |name: &str, body: &str| {
            let path = temp.path().join(name);
            std::fs::write(&path, body).unwrap();
            path
        };
        let killable = stub("slow.py", "import sys, time\nsys.stdin.read()\ntime.sleep(6)\n");
        let ignores_term = stub("ignores-term.py",
            "import signal, sys, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nsys.stdin.read()\ntime.sleep(6)\n");
        let unkillable = temp.path().join("unkillable.py");
        write_stalled_checkpoint(&unkillable, 6);
        for script in [&killable, &ignores_term, &unkillable] {
            let (code, elapsed, stderr) = run_hook(&journal_hook_command(script, &journal, 1, 1), &event, None);
            assert_eq!(code, Some(2), "{script:?}: {stderr}");
            assert!(elapsed < bound, "{script:?} answered after {elapsed:?}");
            assert!(stderr.contains("did not finish in time"), "{script:?}: {stderr}");
        }

        // Nothing the checkpoint writes can stand in for its exit status: not
        // a success marker placed where the relay's size cap cuts its output,
        // and not a write to any descriptor the wrapper might read a status from.
        let forged = stub("forged.py", "import os, sys\nsys.stdin.read()\n\
            mark = 'jarvis-handoff-hook-exit:0'\n\
            sys.stderr.write('x' * (65536 - len(mark)) + mark + 'y' * 1000)\nsys.stderr.flush()\n\
            for fd in range(3, 10):\n    try:\n        os.write(fd, b'0')\n    except OSError:\n        pass\n\
            sys.exit(1)\n");
        let (code, elapsed, _) = run_hook(&journal_hook_command(&forged, &journal, 1, 1), &event, None);
        assert_eq!(code, Some(2), "a forged success marker must not pass");
        assert!(elapsed < bound, "answered after {elapsed:?}");

        // The checkpoint cannot start: missing script, interpreter or wrapper.
        let missing = temp.path().join("missing.py");
        let (code, _, _) = run_hook(&journal_hook_command(&missing, &journal, 1, 1), &event, None);
        assert_eq!(code, Some(2));
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let (code, _, _) = run_hook(&journal_hook_command(&killable, &journal, 1, 1), &event, Some(bin.as_os_str()));
        assert_eq!(code, Some(2), "no timeout, head or python3");
        for tool in ["timeout", "head"] {
            let found = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|dir| dir.join(tool)).find(|path| path.is_file()).unwrap();
            std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
        }
        let (code, _, stderr) = run_hook(&journal_hook_command(&killable, &journal, 1, 1), &event, Some(bin.as_os_str()));
        assert_eq!(code, Some(2), "no python3");
        assert!(stderr.contains("reconciliation required"), "{stderr}");

        // A real checkpoint answers promptly, and a bridge refusal keeps its message.
        let bridge = stub("bridge.py", include_str!("../../../scripts/codex-tool-bridge.py"));
        let command = journal_hook_command(&bridge, &journal, 1, 1);
        let (code, _, stderr) = run_hook(&command, &event, None);
        assert_eq!((code, stderr.as_str()), (Some(0), ""));
        let (code, _, stderr) = run_hook(&command, &primary_event("PreToolUse", "synthetic-retry"), None);
        assert_eq!((code, stderr.as_str()),
            (Some(2), "Handoff checkpoint unavailable or uncertain; reconciliation required.\n"));
    }

    /// C3: a post-tool checkpoint that fails leaves the operation uncertain;
    /// nothing replays it and nothing records it as completed.
    #[test]
    fn failed_post_tool_checkpoint_leaves_the_operation_uncertain() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = temp.path().join("operations.json");
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.handoff_path = Some(journal.clone());
        let launch = ClaudeHooks::prepare(&opts).unwrap().unwrap();
        let settings: Value = serde_json::from_str(&launch.settings_json).unwrap();
        let command = |event: &str| settings["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap().to_owned();
        let status = || -> Value {
            serde_json::from_slice::<Value>(&std::fs::read(&journal).unwrap()).unwrap()["operations"][0]["status"].clone()
        };
        let (code, _, stderr) = run_hook(&command("PreToolUse"), &primary_event("PreToolUse", "synthetic-1"), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(status(), "started");
        let mut finished = primary_event("PostToolUse", "synthetic-1");
        finished["tool_response"] = json!({"content": [{"type": "text", "text": "synthetic-receipt"}]});

        // The journal is busy (another checkpoint holds its lock).
        let lock = hold_lock(&temp.path().join("operations.json.lock"));
        let (code, _, stderr) = run_hook(&command("PostToolUse"), &finished, None);
        drop(lock);
        assert_eq!(code, Some(2));
        assert!(stderr.contains("reconciliation required") && !stderr.contains("synthetic"), "{stderr}");
        assert_eq!(status(), "started");

        // The checkpoint cannot be written (root ignores directory modes).
        if euid() != 0 {
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
            let (code, _, stderr) = run_hook(&command("PostToolUse"), &finished, None);
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(code, Some(2), "{stderr}");
            assert_eq!(status(), "started");
        }

        // The interpreter is unavailable.
        let (code, _, _) = run_hook(&command("PostToolUse"), &finished,
            Some(std::ffi::OsStr::new("/nonexistent")));
        assert_eq!(code, Some(2));
        assert_eq!(status(), "started");

        // A failed tool is not evidence that its effect is absent: it is
        // recorded as failed (seen by the primary, effect still uncertain).
        let (code, _, stderr) = run_hook(&command("PostToolUseFailure"),
            &primary_event("PostToolUseFailure", "synthetic-1"), None);
        assert_eq!(code, Some(0), "{stderr}");
        assert_eq!(status(), "failed");

        // Replay is refused: a retried call is blocked and the receipt is unchanged.
        let before = std::fs::read(&journal).unwrap();
        let (code, _, stderr) = run_hook(&command("PreToolUse"), &primary_event("PreToolUse", "synthetic-retry"), None);
        assert_eq!(code, Some(2));
        assert!(stderr.contains("reconciliation required"), "{stderr}");
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        // The fallback provider receives it as uncertain, and retention keeps it.
        assert!(resume_message(&journal, "synthetic request").unwrap().contains("\"status\":\"failed\""));
        assert!(!request_finished(&journal).unwrap());
    }

    // ---- #1035 retention ----

    use std::time::{Duration, SystemTime};

    const GRACE: Duration = Duration::from_secs(24 * 3600);
    const TWO_DAYS: Duration = Duration::from_secs(48 * 3600);

    fn private_root() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = temp.path().join("reasoner-handoffs");
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        (temp, root)
    }

    fn write_private(path: &Path, contents: &str) {
        let mut file = std::fs::OpenOptions::new().create(true).truncate(true).write(true)
            .mode(0o600).open(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    /// A request directory exactly as dispatch addresses it, optionally with journal rows.
    fn request(root: &Path, turn: &str, rows: Option<Value>) -> PathBuf {
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.session_id = Some(turn.into());
        let journal = request_path(root, &opts).unwrap();
        if let Some(rows) = rows {
            write_private(&journal, &json!({"version": 1, "operations": rows}).to_string());
        }
        journal
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let since = when.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let time = libc::timespec { tv_sec: since.as_secs() as _, tv_nsec: since.subsec_nanos() as _ };
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let times = [time, time];
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }, 0);
    }

    /// Make a request look idle for `by`: every entry (links not followed), then the directory.
    fn age(journal: &Path, by: Duration) {
        let when = SystemTime::now() - by;
        let directory = journal.parent().unwrap();
        for entry in std::fs::read_dir(directory).unwrap() {
            set_mtime(&entry.unwrap().path(), when);
        }
        set_mtime(directory, when);
    }

    fn hold_lock(path: &Path) -> std::fs::File {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(path).unwrap();
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        file
    }

    fn completed_row() -> Value {
        json!({"tool": "mcp__fixture__create", "arguments": {"title": "Synthetic"}, "primary_id": "synthetic-1",
            "status": "completed", "result": {"content": [{"type": "text", "text": "synthetic-42"}]}})
    }

    fn not_applied_row() -> Value {
        json!({"tool": "mcp__fixture__update", "arguments": {}, "status": "not_applied",
            "reconciliation": {"outcome": "not_applied", "evidence": "Synthetic service shows no change.",
                "prior_fingerprint": "synthetic", "recorded_at": "2026-01-01T00:00:00+00:00"}})
    }

    fn started_row() -> Value {
        json!({"tool": "mcp__fixture__send", "arguments": {"to": "synthetic"}, "primary_id": "synthetic-2", "status": "started"})
    }

    fn gone(journal: &Path) -> bool {
        matches!(std::fs::symlink_metadata(journal.parent().unwrap()), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    }

    #[test]
    fn sweep_removes_only_finished_journals_older_than_grace() {
        let (_temp, root) = private_root();
        // (a) finished and idle past grace, including a request that never recorded a receipt.
        let finished_old = request(&root, "synthetic-finished-old", Some(json!([completed_row(), not_applied_row()])));
        let lock_only_old = request(&root, "synthetic-lock-only-old", None);
        write_private(&lock_only_old.with_extension("lifecycle-lock"), "");
        // (b) finished inside grace.
        let finished_young = request(&root, "synthetic-finished-young", Some(json!([completed_row()])));
        // (c) an uncertain (started) operation.
        let uncertain_old = request(&root, "synthetic-uncertain-old", Some(json!([completed_row(), started_row()])));
        // (d) an in-flight or cleanup-unverified invocation.
        let active_old = request(&root, "synthetic-active-old", Some(json!([completed_row()])));
        write_private(&active_old.with_extension("active"),
            &json!({"version": 1, "receipt": "/nonexistent/synthetic-cleanup-complete"}).to_string());
        // Anything this code does not positively recognise as settled is unfinished.
        let row = |status: Value| json!({"tool": "t", "arguments": {}, "status": status});
        let mut unrecognised = Vec::new();
        for (turn, contents) in [
            ("synthetic-literal-uncertain", json!({"version": 1, "operations": [row(json!("uncertain"))]}).to_string()),
            ("synthetic-future-status", json!({"version": 1, "operations": [completed_row(), row(json!("failed"))]}).to_string()),
            ("synthetic-missing-status", json!({"version": 1, "operations": [{"tool": "t", "arguments": {}}]}).to_string()),
            ("synthetic-completed-without-result", json!({"version": 1, "operations": [row(json!("completed"))]}).to_string()),
            ("synthetic-unproven-absence", json!({"version": 1, "operations": [row(json!("not_applied"))]}).to_string()),
            ("synthetic-future-version", json!({"version": 2, "operations": []}).to_string()),
            ("synthetic-corrupt", "{\"version\": 1, \"operations\": [".to_string()),
        ] {
            let journal = request(&root, turn, None);
            write_private(&journal, &contents);
            unrecognised.push(journal);
        }
        for journal in [&finished_old, &lock_only_old, &uncertain_old, &active_old].into_iter().chain(&unrecognised) {
            age(journal, TWO_DAYS);
        }

        let report = sweep_finished(&root, GRACE, false).unwrap();

        assert!(gone(&finished_old) && gone(&lock_only_old));
        for kept in [&finished_young, &uncertain_old, &active_old].into_iter().chain(&unrecognised) {
            assert!(kept.exists(), "{kept:?} must survive the sweep");
        }
        assert!(active_old.with_extension("active").exists());
        assert_eq!((report.removed, report.kept_recent, report.kept_active, report.kept_unfinished), (2, 1, 1, 8));
        assert_eq!((report.entries, report.kept()), (12, 10));
    }

    /// #1071 — an orphan left by a daemon that died mid-call is cleared and
    /// rejoins the sweep; a live call's and a pre-#1071 marker are kept.
    #[test]
    fn the_orphan_pass_clears_only_provably_dead_markers_and_reports_the_rest() {
        let (_temp, root) = private_root();
        let marker = |journal: &Path, contents: Value| {
            write_private(&journal.with_extension("active"), &contents.to_string());
        };
        let identity = |boot: &str, pid: libc::pid_t| json!({"version": 2,
            "receipt": "/nonexistent/synthetic-cleanup-complete", "boot_id": boot, "writer_pid": pid,
            "writer_start": 4242, "writer_cgroup": "0::/synthetic.slice/augmentagent.service"});
        let orphan = request(&root, "synthetic-orphan", Some(json!([completed_row()])));
        marker(&orphan, identity("a-previous-boot", 999));
        let uncertain = request(&root, "synthetic-orphan-uncertain", Some(json!([completed_row(), started_row()])));
        marker(&uncertain, identity("a-previous-boot", 999));
        let live = request(&root, "synthetic-in-flight", Some(json!([completed_row()])));
        marker(&live, identity("this-boot", 1234));
        let legacy = request(&root, "synthetic-legacy-marker", Some(json!([completed_row()])));
        marker(&legacy, json!({"version": 1, "receipt": "/nonexistent/synthetic-cleanup-complete"}));
        for journal in [&orphan, &uncertain, &live, &legacy] {
            age(journal, TWO_DAYS);
        }
        let env = LivenessEnv::injected(Some("this-boot".into()), None, None, Box::new(|pid| (pid == 1234).then_some(4242)));

        let dry = clear_orphaned_markers(&root, &env, true).unwrap();
        assert_eq!(dry, OrphanReport { cleared: 2, kept_live: 1, kept_unproven: 1 });
        assert!(orphan.with_extension("active").exists(), "a dry run must change nothing");

        let report = clear_orphaned_markers(&root, &env, false).unwrap();
        assert_eq!(report, dry);
        assert!(!orphan.with_extension("active").exists() && !uncertain.with_extension("active").exists());
        assert!(live.with_extension("active").exists() && legacy.with_extension("active").exists());

        // The cleared requests rejoin the sweep from the start of a fresh
        // grace period (removing the marker moved the directory's mtime): the
        // settled one is then removable, the one with a `started` row stays
        // for the recovery command.
        assert_eq!(sweep_finished(&root, GRACE, true).unwrap().kept_recent, 2);
        for journal in [&orphan, &uncertain] {
            age(journal, TWO_DAYS);
        }
        let swept = sweep_finished(&root, GRACE, false).unwrap();
        assert!(gone(&orphan), "a cleared orphan must become eligible for the sweep");
        assert!(uncertain.exists() && std::fs::read_to_string(&uncertain).unwrap().contains("\"started\""));
        assert_eq!((swept.removed, swept.kept_active, swept.kept_unfinished), (1, 2, 1));
    }

    #[test]
    fn sweep_never_follows_symlinks_and_refuses_non_private_state() {
        use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
        let (temp, root) = private_root();
        // A finished, expired journal outside the root, reachable only through links.
        let (_outside_temp, outside) = private_root();
        let target = request(&outside, "synthetic-outside", Some(json!([completed_row()])));
        age(&target, TWO_DAYS);
        let target_dir = target.parent().unwrap();
        let linked_dir = root.join(target_dir.file_name().unwrap());
        symlink(target_dir, &linked_dir).unwrap();
        let linked_journal = request(&root, "synthetic-linked-journal", None);
        symlink(&target, &linked_journal).unwrap();
        let public_dir = request(&root, "synthetic-public-dir", Some(json!([completed_row()])));
        let public_journal = request(&root, "synthetic-public-journal", Some(json!([completed_row()])));
        std::fs::set_permissions(&public_journal, std::fs::Permissions::from_mode(0o640)).unwrap();
        let nested = request(&root, "synthetic-nested", Some(json!([completed_row()])));
        std::fs::DirBuilder::new().mode(0o700).create(nested.parent().unwrap().join("unexpected")).unwrap();
        let control = request(&root, "synthetic-control", Some(json!([completed_row()])));
        for journal in [&linked_journal, &public_dir, &public_journal, &nested, &control] {
            age(journal, TWO_DAYS);
        }
        std::fs::set_permissions(public_dir.parent().unwrap(), std::fs::Permissions::from_mode(0o750)).unwrap();
        set_mtime(&linked_dir, SystemTime::now() - TWO_DAYS);

        let report = sweep_finished(&root, GRACE, false).unwrap();

        assert!(gone(&control), "positive control");
        assert!(target.exists(), "a link must never lead the sweep outside its root");
        assert!(std::fs::symlink_metadata(&linked_dir).unwrap().file_type().is_symlink());
        assert!(std::fs::symlink_metadata(&linked_journal).unwrap().file_type().is_symlink());
        assert!(public_dir.exists() && public_journal.exists() && nested.exists());
        assert_eq!((report.removed, report.kept_untrusted), (1, 5));

        // A root that is a link, or that other users can read, is refused outright.
        let linked_root = temp.path().join("linked-root");
        symlink(&outside, &linked_root).unwrap();
        assert!(sweep_finished(&linked_root, GRACE, false).unwrap_err().to_string().contains("not private"));
        assert!(target.exists());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(sweep_finished(&root, GRACE, false).unwrap_err().to_string().contains("not private"));
        assert!(public_journal.exists());
        // No root yet (fresh box): nothing to do.
        assert_eq!(sweep_finished(&temp.path().join("absent"), GRACE, false).unwrap(), SweepReport::default());
    }

    #[test]
    fn sweep_skips_a_request_whose_lock_is_held() {
        let (_temp, root) = private_root();
        let lifecycle_held = request(&root, "synthetic-lifecycle-held", Some(json!([completed_row()])));
        let lifecycle_guard = hold_lock(&lifecycle_held.with_extension("lifecycle-lock"));
        let journal_held = request(&root, "synthetic-journal-lock-held", Some(json!([completed_row()])));
        let journal_guard = hold_lock(&PathBuf::from(format!("{}.lock", journal_held.display())));
        let control = request(&root, "synthetic-unlocked", Some(json!([completed_row()])));
        for journal in [&lifecycle_held, &journal_held, &control] {
            age(journal, TWO_DAYS);
        }

        let report = sweep_finished(&root, GRACE, false).unwrap();
        assert!(lifecycle_held.exists() && journal_held.exists());
        assert!(gone(&control));
        assert_eq!((report.removed, report.kept_busy), (1, 2));

        // Contention defers removal; it does not reset the request's age.
        drop((lifecycle_guard, journal_guard));
        let report = sweep_finished(&root, GRACE, false).unwrap();
        assert_eq!((report.removed, report.kept()), (2, 0));
        assert!(gone(&lifecycle_held) && gone(&journal_held));
    }

    #[test]
    fn dry_run_reports_the_plan_without_creating_or_removing_anything() {
        let (_temp, root) = private_root();
        let finished = request(&root, "synthetic-dry-finished", Some(json!([completed_row()])));
        // Never reached provider startup, so a real sweep would have to create its lock.
        let bare = request(&root, "synthetic-dry-bare", Some(json!([completed_row()])));
        let _ = std::fs::remove_file(bare.with_extension("lifecycle-lock"));
        let uncertain = request(&root, "synthetic-dry-uncertain", Some(json!([started_row()])));
        for journal in [&finished, &bare, &uncertain] {
            age(journal, TWO_DAYS);
        }
        fn listing(root: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
            let mut out = Vec::new();
            for dir in std::fs::read_dir(root).unwrap() {
                let dir = dir.unwrap().path();
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    out.push((path.clone(), std::fs::symlink_metadata(&path).unwrap()));
                }
                out.push((dir.clone(), std::fs::symlink_metadata(&dir).unwrap()));
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
        let fingerprint = |root: &Path| listing(root).into_iter()
            .map(|(path, m)| (path, m.modified().unwrap(), m.len())).collect::<Vec<_>>();
        let before = fingerprint(&root);

        let report = sweep_finished(&root, GRACE, true).unwrap();

        assert_eq!(fingerprint(&root), before, "a dry run must not write, lock-create or delete");
        assert_eq!((report.entries, report.removed, report.kept_unfinished), (3, 2, 1));
        assert!(report.removed_bytes > 0 && report.bytes > report.removed_bytes);
    }

    #[test]
    fn a_request_addressed_again_restarts_its_retention_clock() {
        // A caller that keeps retrying one turn (every provider latched for
        // days, say) must find its receipts when a provider is next available.
        let (_temp, root) = private_root();
        let retried = request(&root, "synthetic-retried-turn", Some(json!([completed_row()])));
        let abandoned = request(&root, "synthetic-abandoned-turn", Some(json!([completed_row()])));
        age(&retried, TWO_DAYS);
        age(&abandoned, TWO_DAYS);
        assert_eq!(request(&root, "synthetic-retried-turn", None), retried);

        let report = sweep_finished(&root, GRACE, false).unwrap();

        assert!(retried.exists());
        assert!(gone(&abandoned));
        assert_eq!((report.removed, report.kept_recent), (1, 1));
    }

    #[test]
    fn daemon_pass_removes_only_after_a_second_unchanged_observation() {
        let (_temp, root) = private_root();
        let replayed = request(&root, "synthetic-replayed-after-restart", Some(json!([completed_row()])));
        let idle = request(&root, "synthetic-idle", Some(json!([completed_row()])));
        age(&replayed, TWO_DAYS);
        age(&idle, TWO_DAYS);
        // A restarted daemon has no observations, however long it was down.
        let mut observed = HashMap::new();
        let first = sweep(&root, GRACE, Pass::Confirm(&mut observed)).unwrap();
        assert_eq!((first.removed, first.kept_pending), (0, 2));
        assert!(replayed.exists() && idle.exists());
        // A channel replays one of those turns before the next pass.
        assert_eq!(request(&root, "synthetic-replayed-after-restart", None), replayed);
        let second = sweep(&root, GRACE, Pass::Confirm(&mut observed)).unwrap();
        assert_eq!((second.removed, second.kept_recent), (1, 1));
        assert!(replayed.exists() && gone(&idle));
        // Losing the observations (another restart) only delays removal.
        observed.clear();
        age(&replayed, TWO_DAYS);
        assert_eq!(sweep(&root, GRACE, Pass::Confirm(&mut observed)).unwrap().kept_pending, 1);
        assert_eq!(sweep(&root, GRACE, Pass::Confirm(&mut observed)).unwrap().removed, 1);
        assert!(gone(&replayed));
    }

    #[test]
    fn request_path_waiting_on_a_sweep_gets_a_live_directory() {
        let (_temp, root) = private_root();
        let journal = request(&root, "synthetic-contended-turn", Some(json!([completed_row()])));
        let lock = hold_lock(&journal.with_extension("lifecycle-lock"));
        let (opened, waiting) = std::sync::mpsc::channel();
        let (resume, paused) = std::sync::mpsc::channel::<()>();
        let waiter = std::thread::spawn({
            let root = root.clone();
            move || {
                // Deterministic interleaving: pause once the waiter holds a
                // descriptor for the lock file, before it takes the lock.
                crate::process_tree::BEFORE_WAITING_LOCK.set(Some(Box::new(move || {
                    opened.send(()).unwrap();
                    paused.recv().unwrap();
                })));
                let mut opts = crate::reasoner::loop_parse_opts();
                opts.session_id = Some("synthetic-contended-turn".into());
                request_path(&root, &opts)
            }
        });
        waiting.recv_timeout(Duration::from_secs(10)).expect("request_path never reached the lifecycle lock");
        // What a sweep does while it holds the lock: remove the request.
        let directory = journal.parent().unwrap();
        for entry in std::fs::read_dir(directory).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
        drop(lock);
        resume.send(()).unwrap();
        let addressed = waiter.join().unwrap().unwrap();
        assert_eq!(addressed, journal);
        assert!(addressed.parent().unwrap().is_dir(), "caller was handed a removed request directory");
        assert!(!addressed.exists());
    }

    #[test]
    fn recovery_cli_still_reconciles_an_uncertain_journal_older_than_grace() {
        use std::process::{Command, Stdio};
        let (temp, root) = private_root();
        let journal = request(&root, "synthetic-uncertain-turn", Some(json!([completed_row(), started_row()])));
        age(&journal, TWO_DAYS);
        let report = sweep_finished(&root, GRACE, false).unwrap();
        assert_eq!((report.removed, report.kept_unfinished), (0, 1));

        let helper = temp.path().join("recovery.py");
        std::fs::write(&helper, include_bytes!("../../../scripts/codex-tool-bridge.py")).unwrap();
        let status = Command::new("python3").arg("-I").arg(&helper)
            .arg("--handoff-status").arg(&journal).output().unwrap();
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
        let rows: Vec<Value> = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(rows[1]["status"], "started");
        let decision = json!({"index": 1, "fingerprint": rows[1]["fingerprint"], "outcome": "completed",
            "evidence": "Synthetic service shows exactly one change.",
            "result": {"content": [{"type": "text", "text": "synthetic-43"}]}});
        let mut recovery = Command::new("python3").arg("-I").arg(&helper)
            .arg("--handoff-reconcile").arg(&journal)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        recovery.stdin.take().unwrap().write_all(decision.to_string().as_bytes()).unwrap();
        let result = recovery.wait_with_output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let state: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        assert_eq!(state["operations"][1]["status"], "completed");

        // Reconciled and then idle past grace, it is finished like any other.
        age(&journal, TWO_DAYS);
        assert_eq!(sweep_finished(&root, GRACE, false).unwrap().removed, 1);
        assert!(gone(&journal));
    }

    #[test]
    fn retention_grace_comes_from_env_and_rejects_zero_or_absurd_values() {
        let hours = |h: u64| Duration::from_secs(h * 3600);
        let floor = hours(3);
        let grace = |raw| retention_from(raw, floor);
        assert_eq!(grace(None), RetentionSetting { grace: hours(24), source: RetentionSource::Default });
        assert_eq!(grace(Some("  ")).source, RetentionSource::Default);
        assert_eq!(grace(Some(" 72 ")), RetentionSetting { grace: hours(72), source: RetentionSource::Env("72".into()) });
        assert_eq!(grace(Some("3")).grace, hours(3));
        assert_eq!(grace(Some("8760")).grace, hours(8760));
        for invalid in ["0", "1", "2", "-5", "1.5", "a day", "8761", "18446744073709551615"] {
            assert_eq!(grace(Some(invalid)), RetentionSetting { grace: hours(24),
                source: RetentionSource::Rejected(invalid.into()) }, "{invalid}");
        }
    }

    /// #1035 review — a dispatched request has its directory but no lifecycle
    /// marker while it waits at the CLI gate, for up to its class's budget.
    /// No accepted grace may be shorter than that wait plus a margin.
    #[test]
    fn retention_grace_floor_exceeds_the_longest_gate_wait() {
        let hours = |h: u64| Duration::from_secs(h * 3600);
        let classes = {
            let text = crate::reasoner::loop_parse_opts();
            let mut read = text.clone();
            read.allowed_tools = vec!["Read".into()];
            let mut write = text.clone();
            write.allowed_tools = vec!["Write".into()];
            let mut agentic = text.clone();
            agentic.allowed_tools = vec!["Bash(true)".into()];
            [text, read, write, agentic]
        };
        // Another test may briefly override the reasoner timeout; compare
        // two reads taken under the same setting.
        let (gate, longest) = (0..1000).find_map(|_| {
            let gate = longest_gate_wait();
            let longest = classes.iter().map(crate::reasoner::reasoner_timeout_for).max().unwrap();
            (gate == longest).then_some((gate, longest))
        }).expect("longest_gate_wait must be the largest class gate budget");
        assert!(retention_floor_for(gate) >= longest + hours(1));
        for wait in [Duration::ZERO, Duration::from_secs(1), hours(2), hours(2) + Duration::from_secs(1), hours(30)] {
            let floor = retention_floor_for(wait);
            assert!(floor >= wait + hours(1) && floor.as_secs() % 3600 == 0, "{wait:?} -> {floor:?}");
        }
        // Default timeouts: one hour base, doubled for write/agentic calls.
        assert_eq!(retention_floor_for(hours(2)), hours(3));
        assert_eq!(retention_floor_for(hours(2) + Duration::from_secs(1)), hours(4));
        // A floor above the default raises the default with it.
        assert_eq!(retention_from(None, hours(30)).grace, hours(30));
        assert_eq!(retention_from(Some("25"), hours(30)).grace, hours(30));
        assert_eq!(retention_from(Some("31"), hours(30)).grace, hours(31));
    }

    /// The bridge saves by writing a `.handoff-*` temp, fsyncing, renaming and
    /// fsyncing the directory, and performs an effect only after that save
    /// returns. A leftover temp is an interrupted save, never an effect the
    /// journal fails to record, so it neither blocks nor survives removal.
    #[test]
    fn leftover_bridge_save_temp_files_follow_their_request() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (_temp, root) = private_root();
        let pending_state = json!({"version": 1, "operations": [completed_row(), started_row()]}).to_string();
        let finished = request(&root, "synthetic-temp-finished", Some(json!([completed_row()])));
        write_private(&finished.parent().unwrap().join(".handoff-a1b2c3d4"), &pending_state);
        let uncertain = request(&root, "synthetic-temp-uncertain", Some(json!([started_row()])));
        write_private(&uncertain.parent().unwrap().join(".handoff-e5f6a7b8"), &pending_state);
        let public = request(&root, "synthetic-temp-public", Some(json!([completed_row()])));
        let public_temp = public.parent().unwrap().join(".handoff-c9d0e1f2");
        write_private(&public_temp, &pending_state);
        std::fs::set_permissions(&public_temp, std::fs::Permissions::from_mode(0o644)).unwrap();
        let linked = request(&root, "synthetic-temp-linked", Some(json!([completed_row()])));
        symlink(&finished, linked.parent().unwrap().join(".handoff-a3b4c5d6")).unwrap();
        for journal in [&finished, &uncertain, &public, &linked] {
            age(journal, TWO_DAYS);
        }

        let report = sweep_finished(&root, GRACE, false).unwrap();

        assert!(gone(&finished), "an interrupted save must not pin a finished request");
        assert!(uncertain.exists() && public.exists() && linked.exists());
        assert!(public_temp.exists());
        assert_eq!((report.removed, report.kept_unfinished, report.kept_untrusted), (1, 1, 2));
    }

    /// #1069 review M1(c) — a request that finished without a summary keeps a
    /// verdict file beside its journal. Once idle past grace the whole request,
    /// verdict included, is removed like any other finished one; the verdict
    /// never makes it untrusted (and so kept forever).
    #[test]
    fn a_completed_without_summary_verdict_follows_its_request() {
        let (_temp, root) = private_root();
        let finished = request(&root, "synthetic-verdict-finished", Some(json!([completed_row()])));
        write_private(&finished.parent().unwrap().join(VERDICT_FILE),
            &json!({"version": 1, "completed": 1}).to_string());
        let young = request(&root, "synthetic-verdict-young", Some(json!([completed_row()])));
        write_private(&young.parent().unwrap().join(VERDICT_FILE),
            &json!({"version": 1, "completed": 1}).to_string());
        age(&finished, TWO_DAYS);

        let dry = sweep_finished(&root, GRACE, true).unwrap();
        assert_eq!((dry.removed, dry.kept_recent, dry.kept_untrusted), (1, 1, 0));
        let report = sweep_finished(&root, GRACE, false).unwrap();

        assert!(gone(&finished), "an expired verdict must not pin its request");
        assert!(young.parent().unwrap().join(VERDICT_FILE).exists(),
            "inside grace the verdict still refuses a retry");
        assert_eq!((report.removed, report.kept_recent, report.kept_untrusted), (1, 1, 0));
    }

    /// #1035 review — doctor's dead-sweep signal comes from this classification.
    #[test]
    fn report_counts_request_dirs_and_finished_journals_a_live_sweep_would_have_removed() {
        use std::os::unix::fs::symlink;
        let (_temp, root) = private_root();
        let hours = |h: u64| Duration::from_secs(h * 3600);
        let overdue: Vec<_> = (0..2).map(|i| {
            let journal = request(&root, &format!("synthetic-overdue-{i}"), Some(json!([completed_row()])));
            age(&journal, TWO_DAYS);
            journal
        }).collect();
        // Past grace by less than two sweep intervals: a live sweep may not have reached it yet.
        let expiring = request(&root, "synthetic-just-expired", Some(json!([completed_row()])));
        age(&expiring, GRACE + SWEEP_INTERVAL + hours(1) / 2);
        let recent = request(&root, "synthetic-recent", Some(json!([completed_row()])));
        let active = request(&root, "synthetic-overdue-but-active", Some(json!([completed_row()])));
        write_private(&active.with_extension("active"), &json!({"version": 1, "receipt": "/nonexistent"}).to_string());
        let uncertain = request(&root, "synthetic-overdue-but-uncertain", Some(json!([started_row()])));
        for journal in [&active, &uncertain] {
            age(journal, TWO_DAYS);
        }
        write_private(&root.join("notes.txt"), "not a request");
        symlink(overdue[0].parent().unwrap(), root.join(format!("{:064x}", 7))).unwrap();

        let report = sweep_finished(&root, GRACE, true).unwrap();

        assert_eq!((report.entries, report.requests), (8, 6));
        assert_eq!((report.removed, report.finished_overdue), (3, 2));
        assert_eq!((report.kept_recent, report.kept_active, report.kept_unfinished, report.kept_untrusted), (1, 1, 1, 2));
        assert!(recent.exists());
    }

    /// #1036 H2: the hourly loop also runs the build-scratch sweep, off the
    /// async runtime, even when the journal root is unknown.
    #[tokio::test(flavor = "current_thread")]
    async fn sweep_loop_also_runs_the_build_scratch_sweep_on_every_tick() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runtime_thread = std::thread::current().id();
        let seen = std::sync::Arc::clone(&calls);
        let off_runtime = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = std::sync::Arc::clone(&off_runtime);
        let also: SweepHook = std::sync::Arc::new(move || {
            flag.fetch_and(std::thread::current().id() != runtime_thread, std::sync::atomic::Ordering::SeqCst);
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(run_sweep_loop(None, GRACE, Duration::from_millis(20), shutdown.clone(), Some(also)));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            assert!(std::time::Instant::now() < deadline, "the scratch sweep did not run on consecutive ticks");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(off_runtime.load(std::sync::atomic::Ordering::SeqCst), "the scratch sweep must run on the blocking pool");
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sweep_loop_runs_at_start_and_stops_on_shutdown() {
        assert!(SWEEP_INTERVAL <= Duration::from_secs(3600), "C2: at least hourly");
        let (_temp, root) = private_root();
        let journal = request(&root, "synthetic-startup-sweep", Some(json!([completed_row()])));
        age(&journal, TWO_DAYS);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(run_sweep_loop(Some(root.clone()), GRACE, Duration::from_millis(50), shutdown.clone(), None));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !gone(&journal) {
            assert!(std::time::Instant::now() < deadline, "startup sweep did not run");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().unwrap();
    }
}
