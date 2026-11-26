//! `augmentagent log-rotate` (#1413).
//!
//! Every unit logs with `StandardOutput=append:` into the state dir and the
//! in-process sinks (`tool-audit.log`, `token-usage.jsonl`) append forever.
//! Nothing rotated any of it. This is the rotation: one command, on a timer,
//! the same on every platform (no host logrotate config).
//!
//! * **When.** A file is rotated once it reaches the size cap, or once a
//!   calendar month has passed since it was last rotated (or created).
//! * **How.** If no process holds the file open it is *renamed*: atomic and
//!   lossless, and the next writer simply creates a fresh file. That covers
//!   the timer units and the sinks, which open per write. A file a process
//!   holds open (the daemon's own stdout/stderr) cannot be renamed out from
//!   under its fd, so it is copied and truncated in place; a line written in
//!   the instant between the two can be lost. A held `.jsonl` is therefore
//!   skipped rather than risk half a record.
//! * **What is kept.** Rotated files are gzip-compressed and kept for
//!   `AUGMENTAGENT_LOG_KEEP_MONTHS` (default 3), then deleted — unless the
//!   ops archive (#1414) is configured, which takes them first.
//! * **Readers** that need history across a rotation use
//!   [`read_with_rotated`] / [`tail_with_rotated`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};

pub const DEFAULT_MAX_MB: u64 = 50;
pub const DEFAULT_KEEP_MONTHS: u32 = 3;
pub const SYNC_DEFAULT_MAX_MB: u64 = 10;
pub const SYNC_DEFAULT_KEEP: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_bytes: u64,
    /// The retention budget: months of history for the state dir's logs, and
    /// — for the sync job logs, which can rotate many times inside one month
    /// — a cap on the number of archives kept (#1431).
    pub keep_months: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self { max_bytes: DEFAULT_MAX_MB * 1024 * 1024, keep_months: DEFAULT_KEEP_MONTHS }
    }
}

impl Policy {
    /// The state dir's policy. The sync-log names are read as a fallback, so a
    /// host that configures only its sync rotation gets one coherent policy
    /// rather than two that disagree.
    pub fn from_env() -> Self {
        Self::default().with_env(
            &["AUGMENTAGENT_LOG_ROTATE_MB", "AUGMENTAGENT_SYNC_LOG_ROTATE_MB"],
            &["AUGMENTAGENT_LOG_KEEP_MONTHS", "AUGMENTAGENT_SYNC_LOG_KEEP"],
        )
    }

    /// The scheduled-sync job logs rotate tighter and keep a fixed number of
    /// archives: 10 MB and 5, from `AUGMENTAGENT_SYNC_LOG_ROTATE_MB` /
    /// `AUGMENTAGENT_SYNC_LOG_KEEP`.
    pub fn sync_from_env() -> Self {
        Self { max_bytes: SYNC_DEFAULT_MAX_MB * 1024 * 1024, keep_months: SYNC_DEFAULT_KEEP }
            .with_env(&["AUGMENTAGENT_SYNC_LOG_ROTATE_MB"], &["AUGMENTAGENT_SYNC_LOG_KEEP"])
    }

    /// The first of `mb_keys` / `keep_keys` the environment sets wins.
    fn with_env(self, mb_keys: &[&str], keep_keys: &[&str]) -> Self {
        let num = |keys: &[&str]| {
            keys.iter()
                .find_map(|key| std::env::var(*key).ok()?.trim().parse::<u64>().ok())
        };
        Self {
            max_bytes: num(mb_keys).map_or(self.max_bytes, |mb| mb * 1024 * 1024),
            keep_months: num(keep_keys).map_or(self.keep_months, |n| n as u32),
        }
    }
}

/// Where the macOS scheduled-sync jobs write (`scripts/*/schedule.py`); nothing
/// rotated these until #1431, and one reached 112 MB. On Linux the same jobs
/// log into the state dir, which this command already covers. Giving launchd
/// its own equivalent of the housekeeping timer that calls this command is the
/// separate parity gap #1406; this pass covers the dir whenever it runs.
pub fn sync_log_dir() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs/augmentagent"))
}

// ---------------------------------------------------------------------------
// Names and decisions (pure).
// ---------------------------------------------------------------------------

/// A live log this command owns: `*.log` or `*.jsonl`, not a rotated file.
pub fn is_rotatable(name: &str) -> bool {
    (name.ends_with(".log") || name.ends_with(".jsonl")) && !name.starts_with('.')
}

/// A scheduled data-sync job's log (`imessage-sync.log`, `wiki-sync.log`, …).
/// A run appends to it through a redirect held open for the whole run, and the
/// file grows in bursts when a per-file failure loop is hot, so these need the
/// inode kept and a history bounded by count rather than by age (#1431).
pub fn is_sync_log(name: &str) -> bool {
    is_rotatable(name) && name.ends_with("-sync.log")
}

/// `stderr.log` rotated on 2026-10-06 → `stderr.log.20261006.gz`.
pub fn rotated_name(name: &str, date: NaiveDate) -> String {
    format!("{name}.{}.gz", date.format("%Y%m%d"))
}

/// The live name and rotation date of a rotated file, `-N` suffix allowed
/// (`stderr.log.20261006-2.gz`, a second rotation on one day).
pub fn parse_rotated(name: &str) -> Option<(&str, NaiveDate)> {
    let stem = name.strip_suffix(".gz")?;
    let (base, stamp) = stem.rsplit_once('.')?;
    let day = stamp.split('-').next()?;
    if day.len() != 8 || !is_rotatable(base) {
        return None;
    }
    Some((base, NaiveDate::parse_from_str(day, "%Y%m%d").ok()?))
}

/// Should a `size`-byte file last rotated (or created) on `since` go now?
pub fn is_due(size: u64, since: Option<NaiveDate>, today: NaiveDate, policy: &Policy) -> bool {
    if size == 0 {
        return false;
    }
    if size >= policy.max_bytes {
        return true;
    }
    since.is_some_and(|d| (d.year(), d.month()) < (today.year(), today.month()))
}

/// Is a file rotated on `date` past local retention?
pub fn is_expired(date: NaiveDate, today: NaiveDate, keep_months: u32) -> bool {
    let months = |d: NaiveDate| d.year() * 12 + d.month() as i32;
    months(today) - months(date) > keep_months as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Nobody holds it: rename, lossless.
    Rename,
    /// A process holds it: copy, then truncate in place.
    CopyTruncate,
    /// A held record log: left alone rather than risk half a record.
    Skip,
}

/// `held`: some process has the file open. `None` = cannot tell, treated as held.
pub fn method_for(name: &str, held: Option<bool>) -> Method {
    // A sync job's log is always copied and truncated, whatever this instant
    // says about its fd: a job that starts between the check and the rename
    // would spend the rest of its run appending to an unlinked file (#1431).
    if is_sync_log(name) {
        return Method::CopyTruncate;
    }
    match (held.unwrap_or(true), name.ends_with(".jsonl")) {
        (false, _) => Method::Rename,
        (true, false) => Method::CopyTruncate,
        (true, true) => Method::Skip,
    }
}

// ---------------------------------------------------------------------------
// Filesystem.
// ---------------------------------------------------------------------------

fn gzip_into(src: &Path, dst: &Path) -> Result<u64> {
    let mut input = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let out = std::fs::File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    let n = std::io::copy(&mut input, &mut enc)?;
    enc.finish()?.sync_all()?;
    Ok(n)
}

/// A name for today's rotation of `name` that does not exist yet, and that
/// sorts after every rotation already taken today. Reusing a name the archive
/// cap has freed would label the newest rotation as the oldest, and the next
/// pass would delete it instead of the stale one (#1431).
fn free_rotated_path(dir: &Path, name: &str, today: NaiveDate) -> PathBuf {
    let taken = rotated_files(dir, name)
        .iter()
        .filter(|(_, date)| *date == today)
        .map(|(path, _)| generation(path))
        .max();
    let Some(highest) = taken else {
        return dir.join(rotated_name(name, today));
    };
    (highest + 1..)
        .map(|n| dir.join(format!("{name}.{}-{n}.gz", today.format("%Y%m%d"))))
        .find(|p| !p.exists())
        .expect("an unbounded range always yields a free name")
}

/// Rotate one file now. Returns the compressed file and the bytes it holds.
pub fn rotate_file(path: &Path, method: Method, today: NaiveDate) -> Result<Option<(PathBuf, u64)>> {
    let dir = path.parent().context("log has no parent dir")?;
    let name = path.file_name().and_then(|n| n.to_str()).context("log name is not UTF-8")?;
    let dst = free_rotated_path(dir, name, today);
    let staging = dir.join(format!(".{name}.rotating"));
    match method {
        Method::Skip => return Ok(None),
        Method::Rename => {
            std::fs::rename(path, &staging)
                .with_context(|| format!("rename {}", path.display()))?;
        }
        Method::CopyTruncate => {
            // Copy what is there, then cut the original back to empty. The
            // writer's fd is O_APPEND, so its next line lands at the new end.
            let mut src = std::fs::File::open(path)?;
            let mut out = std::fs::File::create(&staging)?;
            std::io::copy(&mut src, &mut out)?;
            // Anything appended while copying: one more short pass.
            std::io::copy(&mut src, &mut out)?;
            out.sync_all()?;
            std::fs::OpenOptions::new().write(true).open(path)?.set_len(0)?;
        }
    }
    let bytes = gzip_into(&staging, &dst)?;
    std::fs::remove_file(&staging)?;
    Ok(Some((dst, bytes)))
}

/// When `name` was last rotated, from its rotated siblings in `dir`.
fn last_rotation(dir: &Path, name: &str) -> Option<NaiveDate> {
    rotated_files(dir, name).last().map(|(_, date)| *date)
}

/// Rotated siblings of `name`, oldest first.
pub fn rotated_files(dir: &Path, name: &str) -> Vec<(PathBuf, NaiveDate)> {
    let mut out: Vec<(PathBuf, NaiveDate)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let file = e.file_name().to_str()?.to_string();
                    let (base, date) = parse_rotated(&file)?;
                    (base == name).then(|| (e.path(), date))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Which rotation of its day an archive is: `a.log.20261120.gz` is the first,
/// `a.log.20261120-2.gz` the second. Same-day archives need this to order —
/// plain path order puts `-2` before `.gz`.
fn generation(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".gz"))
        .and_then(|n| n.rsplit_once('.'))
        .and_then(|(_, stamp)| stamp.split_once('-'))
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(1)
}

/// Keep the newest `keep` rotations of `name` and delete the rest. Only files
/// this command named are candidates, so a hand-made archive sitting beside
/// them (`imessage-sync.log.fda-spam-20260928.gz`) is never touched.
fn prune_archives(dir: &Path, name: &str, keep: u32, dry_run: bool) -> Vec<Outcome> {
    let mut archives = rotated_files(dir, name);
    archives.sort_by_key(|(path, date)| (*date, generation(path)));
    let excess = archives.len().saturating_sub(keep as usize);
    archives
        .into_iter()
        .take(excess)
        .map(|(path, _)| {
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let action = if dry_run {
                "would delete (over the retained archive count)".to_string()
            } else if std::fs::remove_file(&path).is_ok() {
                "deleted (over the retained archive count)".to_string()
            } else {
                "failed to delete".to_string()
            };
            Outcome { file: path.file_name().unwrap_or_default().to_string_lossy().into_owned(), action, bytes }
        })
        .collect()
}

fn created_on(path: &Path) -> Option<NaiveDate> {
    let t = std::fs::metadata(path).ok()?.created().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(t).date_naive())
}

/// Paths under `dir` that some running process has open. `None` without `/proc`.
fn open_files_in(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) {
            out.extend(
                fds.flatten()
                    .filter_map(|fd| std::fs::read_link(fd.path()).ok())
                    .filter(|target| target.starts_with(dir)),
            );
        }
    }
    Some(out)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Outcome {
    pub file: String,
    pub action: String,
    pub bytes: u64,
}

/// Rotate what is due in `dir` and expire rotated files past retention.
/// `keep_expired` leaves expiry to the caller (the ops archive takes them).
pub fn run(dir: &Path, policy: &Policy, today: NaiveDate, dry_run: bool, keep_expired: bool) -> Vec<Outcome> {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let open = open_files_in(&canon);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().map(String::from)).collect())
        .unwrap_or_default();
    names.sort();
    let mut out = Vec::new();

    for name in names.iter().filter(|n| is_rotatable(n)) {
        let path = dir.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        let since = last_rotation(dir, name).or_else(|| created_on(&path));
        if !is_due(meta.len(), since, today, policy) {
            continue;
        }
        let held = open.as_ref().map(|paths| paths.contains(&canon.join(name)));
        let method = method_for(name, held);
        let action = match method {
            Method::Skip => "skipped: a process holds this record log open".to_string(),
            _ if dry_run => format!("would rotate ({method:?})"),
            _ => match rotate_file(&path, method, today) {
                Ok(Some((dst, _))) => format!(
                    "rotated to {} ({method:?})",
                    dst.file_name().unwrap_or_default().to_string_lossy()
                ),
                Ok(None) => continue,
                Err(e) => format!("failed: {e:#}"),
            },
        };
        out.push(Outcome { file: name.clone(), action, bytes: meta.len() });
    }

    if !keep_expired {
        for name in &names {
            let Some((_, date)) = parse_rotated(name) else { continue };
            if !is_expired(date, today, policy.keep_months) {
                continue;
            }
            let path = dir.join(name);
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let action = if dry_run {
                "would delete (past local retention)".to_string()
            } else if std::fs::remove_file(&path).is_ok() {
                "deleted (past local retention)".to_string()
            } else {
                "failed to delete".to_string()
            };
            out.push(Outcome { file: name.clone(), action, bytes });
        }
        // Size rotations can fire many times inside one month, so the sync
        // job logs are capped by archive count as well (#1431).
        let mut sync_logs: Vec<&str> = names
            .iter()
            .filter_map(|n| if is_sync_log(n) { Some(n.as_str()) } else { parse_rotated(n).map(|(base, _)| base) })
            .filter(|base| is_sync_log(base))
            .collect();
        sync_logs.sort();
        sync_logs.dedup();
        for name in sync_logs {
            out.extend(prune_archives(dir, name, policy.keep_months, dry_run));
        }
    }
    out
}

pub fn run_cli(dir: &Path, dry_run: bool, json: bool, keep_expired: bool, out: &mut dyn Write) -> Result<()> {
    let today = chrono::Utc::now().date_naive();
    let mut outcomes = run(dir, &Policy::from_env(), today, dry_run, keep_expired);
    // The macOS sync jobs log outside the state dir, and nothing rotated them
    // at all until #1431; this pass covers them on their own tighter policy.
    if let Some(sync_dir) = sync_log_dir().filter(|d| d.as_path() != dir && d.is_dir()) {
        outcomes.extend(run(&sync_dir, &Policy::sync_from_env(), today, dry_run, keep_expired));
    }
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&outcomes)?)?;
        return Ok(());
    }
    for o in &outcomes {
        writeln!(out, "{:>8.1} MB  {}  {}", o.bytes as f64 / (1024.0 * 1024.0), o.file, o.action)?;
    }
    if outcomes.is_empty() {
        writeln!(out, "nothing due in {}", dir.display())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Readers across a rotation boundary.
// ---------------------------------------------------------------------------

fn gunzip(path: &Path) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).ok()?).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// The live file preceded by its locally kept rotations, oldest first.
/// For readers that roll up history (token usage).
pub fn read_with_rotated(path: &Path) -> String {
    let mut buf = Vec::new();
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) {
        for (rotated, _) in rotated_files(dir, name) {
            buf.extend(gunzip(&rotated).unwrap_or_default());
        }
    }
    buf.extend(std::fs::read(path).unwrap_or_default());
    String::from_utf8_lossy(&buf).into_owned()
}

/// The last `max` bytes of a log. When the live file is shorter than that
/// (it was just rotated), the tail of the newest rotation fills the gap, so a
/// reader looking back an hour is not blind for the hour after a rotation.
pub fn tail_with_rotated(path: &Path, max: u64) -> Option<String> {
    let live = std::fs::read(path).ok();
    let mut buf = live.clone().unwrap_or_default();
    if (buf.len() as u64) < max {
        let newest = path
            .parent()
            .zip(path.file_name().and_then(|n| n.to_str()))
            .and_then(|(dir, name)| rotated_files(dir, name).pop());
        if let Some(mut older) = newest.and_then(|(p, _)| gunzip(&p)) {
            older.extend(buf);
            buf = older;
        } else if live.is_none() {
            return None;
        }
    }
    let start = buf.len().saturating_sub(max as usize);
    Some(String::from_utf8_lossy(&buf[start..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn rotated_names_round_trip_and_live_names_do_not_parse() {
        let name = rotated_name("stderr.log", d(2026, 10, 6));
        assert_eq!(name, "stderr.log.20261006.gz");
        assert_eq!(parse_rotated(&name), Some(("stderr.log", d(2026, 10, 6))));
        assert_eq!(parse_rotated("token-usage.jsonl.20261006-2.gz"), Some(("token-usage.jsonl", d(2026, 10, 6))));
        assert_eq!(parse_rotated("stderr.log"), None);
        assert_eq!(parse_rotated("notes.20261006.gz"), None, "not one of ours");
        assert_eq!(parse_rotated("stderr.log.2026.gz"), None);
        assert!(is_rotatable("stderr.log") && is_rotatable("token-usage.jsonl"));
        assert!(!is_rotatable("stderr.log.20261006.gz") && !is_rotatable("built-commit"));
        assert!(!is_rotatable(".stderr.log.rotating"), "the staging file is never picked up");
    }

    #[test]
    fn due_on_size_or_once_a_calendar_month_has_passed() {
        let p = Policy { max_bytes: 100, keep_months: 3 };
        let today = d(2026, 10, 6);
        assert!(!is_due(0, Some(d(2026, 1, 1)), today, &p), "an empty file never rotates");
        assert!(is_due(100, Some(today), today, &p), "at the cap");
        assert!(!is_due(99, Some(d(2026, 10, 1)), today, &p), "same month, under the cap");
        assert!(is_due(1, Some(d(2026, 9, 30)), today, &p), "last rotated in an earlier month");
        assert!(is_due(1, Some(d(2025, 12, 31)), d(2026, 1, 1), &p), "across a year boundary");
        assert!(!is_due(99, None, today, &p), "no known age: only the cap decides");
    }

    #[test]
    fn expiry_counts_whole_months() {
        let today = d(2026, 10, 6);
        assert!(!is_expired(d(2026, 7, 1), today, 3), "three months back is kept");
        assert!(is_expired(d(2026, 6, 30), today, 3));
        assert!(!is_expired(d(2025, 11, 1), d(2026, 2, 1), 3), "across a year boundary");
        assert!(is_expired(d(2026, 9, 30), today, 0), "keep 0 keeps only this month");
    }

    #[test]
    fn a_held_text_log_is_copied_and_a_held_record_log_is_left_alone() {
        assert_eq!(method_for("stderr.log", Some(false)), Method::Rename);
        assert_eq!(method_for("token-usage.jsonl", Some(false)), Method::Rename);
        assert_eq!(method_for("stderr.log", Some(true)), Method::CopyTruncate);
        assert_eq!(method_for("token-usage.jsonl", Some(true)), Method::Skip);
        assert_eq!(method_for("stderr.log", None), Method::CopyTruncate, "cannot tell: assume held");
        assert_eq!(method_for("token-usage.jsonl", None), Method::Skip);
        // A sync job's log keeps its inode however its fd looks right now.
        assert_eq!(method_for("wiki-sync.log", Some(false)), Method::CopyTruncate);
        assert!(is_sync_log("imessage-sync.log") && is_sync_log("apple-notes-sync.log"));
        assert!(!is_sync_log("imessage-sync.log.20261120.gz") && !is_sync_log("stderr.log"));
    }

    fn read_gz(path: &Path) -> String {
        String::from_utf8(gunzip(path).unwrap()).unwrap()
    }

    #[test]
    fn rename_rotation_loses_no_record_and_writers_start_a_fresh_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("token-usage.jsonl");
        let lines: Vec<String> = (0..500).map(|i| format!("{{\"n\":{i}}}")).collect();
        std::fs::write(&log, lines.join("\n") + "\n").unwrap();

        let (dst, bytes) = rotate_file(&log, Method::Rename, d(2026, 10, 6)).unwrap().unwrap();
        assert_eq!(dst.file_name().unwrap(), "token-usage.jsonl.20261006.gz");
        assert!(!log.exists(), "the live name is free for the next writer");
        assert!(!tmp.path().join(".token-usage.jsonl.rotating").exists());
        let back = read_gz(&dst);
        assert_eq!(bytes as usize, back.len());
        let parsed: Vec<&str> = back.lines().collect();
        assert_eq!(parsed.len(), 500, "every record, none split");
        assert!(parsed.iter().all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()));

        // The sink opens per write with create+append.
        std::fs::write(&log, "{\"n\":500}\n").unwrap();
        let all = read_with_rotated(&log);
        assert_eq!(all.lines().count(), 501, "a reader sees across the boundary, in order");
        assert!(all.ends_with("{\"n\":500}\n"));
    }

    #[test]
    fn copy_truncate_keeps_the_writers_fd_working() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("stderr.log");
        // The unit's `append:` fd, open for the whole run.
        let mut held = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writeln!(held, "before rotation").unwrap();

        let (dst, _) = rotate_file(&log, Method::CopyTruncate, d(2026, 10, 6)).unwrap().unwrap();
        assert_eq!(read_gz(&dst), "before rotation\n");
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0, "cut back in place");

        writeln!(held, "after rotation").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "after rotation\n", "no hole, no stale offset");
    }

    #[test]
    fn a_second_rotation_on_one_day_gets_its_own_name() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        std::fs::write(&log, "one\n").unwrap();
        let (first, _) = rotate_file(&log, Method::Rename, d(2026, 10, 6)).unwrap().unwrap();
        std::fs::write(&log, "two\n").unwrap();
        let (second, _) = rotate_file(&log, Method::Rename, d(2026, 10, 6)).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap(), "a.log.20261006-2.gz");
        assert_eq!(read_gz(&first), "one\n");
        assert_eq!(rotated_files(tmp.path(), "a.log").len(), 2);
    }

    #[test]
    fn a_pass_rotates_what_is_due_expires_old_rotations_and_leaves_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let today = d(2026, 10, 6);
        let policy = Policy { max_bytes: 1000, keep_months: 3 };
        std::fs::write(dir.join("big.log"), vec![b'x'; 2000]).unwrap();
        std::fs::write(dir.join("small.log"), b"tiny\n").unwrap();
        std::fs::write(dir.join("empty.log"), b"").unwrap();
        std::fs::write(dir.join("built-commit"), b"abc").unwrap();
        // Last rotated in September: due on the month rule though small.
        std::fs::write(dir.join("monthly.log"), b"line\n").unwrap();
        std::fs::write(dir.join(rotated_name("monthly.log", d(2026, 9, 2))), b"gz").unwrap();
        std::fs::write(dir.join(rotated_name("monthly.log", d(2026, 5, 2))), b"gz").unwrap();

        let dry = run(dir, &policy, today, true, false);
        assert_eq!(dry.len(), 3, "{dry:?}");
        assert!(dir.join("big.log").metadata().unwrap().len() == 2000, "a dry run changes nothing");

        let done = run(dir, &policy, today, false, false);
        let acted: Vec<&str> = done.iter().map(|o| o.file.as_str()).collect();
        assert_eq!(acted, ["big.log", "monthly.log", "monthly.log.20260502.gz"]);
        assert!(dir.join("big.log.20261006.gz").exists() && !dir.join("big.log").exists());
        assert!(dir.join("monthly.log.20261006.gz").exists());
        assert!(dir.join("monthly.log.20260902.gz").exists(), "inside retention");
        assert!(!dir.join("monthly.log.20260502.gz").exists(), "past retention");
        assert_eq!(std::fs::read(dir.join("small.log")).unwrap(), b"tiny\n");
        assert!(dir.join("empty.log").exists() && dir.join("built-commit").exists());

        // The archive takes expired files itself when it is configured.
        std::fs::write(dir.join(rotated_name("small.log", d(2026, 1, 1))), b"gz").unwrap();
        run(dir, &policy, today, false, true);
        assert!(dir.join("small.log.20260101.gz").exists(), "keep_expired leaves expiry to the caller");
    }

    /// #1431 — nothing rotated the scheduled-sync logs and one reached 112 MB.
    #[test]
    fn a_sync_log_is_compressed_in_place_so_the_running_job_keeps_appending() {
        use std::io::Write as _;
        use std::os::unix::fs::MetadataExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let log = dir.join("imessage-sync.log");
        let policy = Policy { max_bytes: SYNC_DEFAULT_MAX_MB * 1024 * 1024, keep_months: SYNC_DEFAULT_KEEP };
        let today = d(2026, 11, 20);
        // The size the issue reported, sparse here. Between runs nothing holds
        // the log, but the job's redirect opens it for the whole of the next
        // run: the inode has to survive regardless.
        std::fs::File::create(&log).unwrap().set_len(112 * 1024 * 1024).unwrap();
        let inode = log.metadata().unwrap().ino();

        run(dir, &policy, today, false, false);
        assert!(log.exists(), "the job's append target is still there");
        assert_eq!(log.metadata().unwrap().ino(), inode, "same inode, not a fresh file");
        assert!(log.metadata().unwrap().len() < policy.max_bytes, "cut back under the cap");
        assert_eq!(rotated_files(dir, "imessage-sync.log").len(), 1);
        assert!(dir.join("imessage-sync.log.20261120.gz").exists(), "and the copy is compressed");

        let mut next_run = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(next_run, "synced 3 messages").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "synced 3 messages\n");
    }

    #[test]
    fn sync_log_history_is_capped_by_count_and_hand_made_archives_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let log = dir.join("whatsapp-sync.log");
        let policy = Policy { max_bytes: 1024, keep_months: 5 };
        let today = d(2026, 11, 20);
        let decoy = dir.join("whatsapp-sync.log.fda-spam-20260928.gz");
        std::fs::write(&decoy, b"archived by hand").unwrap();

        // Six size rotations inside one day: the month rule expires none.
        for _ in 0..6 {
            std::fs::write(&log, vec![b'x'; 2048]).unwrap();
            run(dir, &policy, today, false, false);
        }
        let kept: Vec<String> = rotated_files(dir, "whatsapp-sync.log")
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(kept.len(), 5, "{kept:?}");
        assert!(!kept.contains(&"whatsapp-sync.log.20261120.gz".to_string()), "the oldest went: {kept:?}");
        assert!(decoy.exists(), "pruning only touches archives rotation named");
    }

    /// #1431 — the cap must drop the oldest rotation, every pass. Today's
    /// plain name sorts first, so a rotation written back into the slot the
    /// cap just freed is labelled the oldest and the next pass deletes it:
    /// the history freezes at the first `keep` rotations and every later one
    /// is thrown away.
    #[test]
    fn the_newest_sync_rotation_survives_once_the_cap_is_reached() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let log = dir.join("imessage-sync.log");
        let policy = Policy { max_bytes: 1024, keep_months: 5 };
        let today = d(2026, 11, 20);

        // Seven size rotations inside one day: one more than the cap keeps.
        for pass in 1..=7 {
            std::fs::write(&log, format!("pass {pass}\n").repeat(200)).unwrap();
            run(dir, &policy, today, false, false);
        }
        let kept: Vec<String> =
            rotated_files(dir, "imessage-sync.log").iter().map(|(p, _)| read_gz(p)).collect();
        assert_eq!(kept.len(), 5, "{kept:?}");
        for pass in 3..=7 {
            assert!(
                kept.iter().any(|body| body.starts_with(&format!("pass {pass}\n"))),
                "the five newest rotations are the ones kept, missing pass {pass}"
            );
        }
    }

    #[test]
    fn the_sync_policy_defaults_to_ten_megabytes_and_five_archives() {
        let saved: Vec<(&str, Option<String>)> = ["AUGMENTAGENT_SYNC_LOG_ROTATE_MB", "AUGMENTAGENT_SYNC_LOG_KEEP"]
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        assert_eq!(Policy::sync_from_env(), Policy { max_bytes: 10 * 1024 * 1024, keep_months: 5 });
        std::env::set_var("AUGMENTAGENT_SYNC_LOG_ROTATE_MB", "2");
        std::env::set_var("AUGMENTAGENT_SYNC_LOG_KEEP", "4");
        assert_eq!(Policy::sync_from_env(), Policy { max_bytes: 2 * 1024 * 1024, keep_months: 4 });
        assert_eq!(Policy::from_env(), Policy { max_bytes: 2 * 1024 * 1024, keep_months: 4 },
                   "a host that configures only its sync logs gets one policy");
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn a_tail_reaches_into_the_newest_rotation_only_when_the_live_file_is_short() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("stderr.log");
        std::fs::write(&log, "old-1\nold-2\n").unwrap();
        rotate_file(&log, Method::Rename, d(2026, 10, 6)).unwrap();
        assert_eq!(tail_with_rotated(&log, 6).as_deref(), Some("old-2\n"), "live file gone: rotation only");
        std::fs::write(&log, "new-1\n").unwrap();
        assert_eq!(tail_with_rotated(&log, 12).as_deref(), Some("old-2\nnew-1\n"));
        assert_eq!(tail_with_rotated(&log, 6).as_deref(), Some("new-1\n"), "long enough: no decompression needed");
        assert_eq!(tail_with_rotated(&tmp.path().join("missing.log"), 10), None);
    }
}
