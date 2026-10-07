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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_bytes: u64,
    pub keep_months: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self { max_bytes: DEFAULT_MAX_MB * 1024 * 1024, keep_months: DEFAULT_KEEP_MONTHS }
    }
}

impl Policy {
    pub fn from_env() -> Self {
        let num = |key: &str| std::env::var(key).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let d = Self::default();
        Self {
            max_bytes: num("AUGMENTAGENT_LOG_ROTATE_MB").map_or(d.max_bytes, |mb| mb * 1024 * 1024),
            keep_months: num("AUGMENTAGENT_LOG_KEEP_MONTHS").map_or(d.keep_months, |m| m as u32),
        }
    }
}

// ---------------------------------------------------------------------------
// Names and decisions (pure).
// ---------------------------------------------------------------------------

/// A live log this command owns: `*.log` or `*.jsonl`, not a rotated file.
pub fn is_rotatable(name: &str) -> bool {
    (name.ends_with(".log") || name.ends_with(".jsonl")) && !name.starts_with('.')
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

/// A name for today's rotation of `name` that does not exist yet.
fn free_rotated_path(dir: &Path, name: &str, today: NaiveDate) -> PathBuf {
    let first = dir.join(rotated_name(name, today));
    if !first.exists() {
        return first;
    }
    (2..)
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
    }
    out
}

pub fn run_cli(dir: &Path, dry_run: bool, json: bool, keep_expired: bool, out: &mut dyn Write) -> Result<()> {
    let today = chrono::Utc::now().date_naive();
    let outcomes = run(dir, &Policy::from_env(), today, dry_run, keep_expired);
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
        assert_eq!(method_for("wiki-sync.log", Some(false)), Method::Rename);
        assert_eq!(method_for("token-usage.jsonl", Some(false)), Method::Rename);
        assert_eq!(method_for("stderr.log", Some(true)), Method::CopyTruncate);
        assert_eq!(method_for("token-usage.jsonl", Some(true)), Method::Skip);
        assert_eq!(method_for("stderr.log", None), Method::CopyTruncate, "cannot tell: assume held");
        assert_eq!(method_for("token-usage.jsonl", None), Method::Skip);
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
