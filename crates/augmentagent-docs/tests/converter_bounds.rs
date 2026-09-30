//! #1293 — the stage-1 converters (pdftotext / pandoc) are resolved the same
//! way for every caller and never hang a turn: a tool missing from the
//! service PATH is a clear error, a tool in a service directory that the
//! launchd PATH lacks is still found, and a stuck converter is killed after
//! the timeout. Fake converters are tiny shell scripts in a temp dir, so the
//! tests run the same on Linux and macOS without poppler or pandoc.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use augmentagent_docs::{
    convert_doc_to_text_with, extract_text_with, resolve_tool, ConvertOptions, DocKind,
};

/// Write an executable script without this process ever holding a write
/// descriptor on it. Tests run on parallel threads that fork children; a
/// child forked while `std::fs::write` has the file open inherits that
/// descriptor until it execs, and on Linux exec'ing a file some process has
/// open for writing fails with ETXTBSY ("Text file busy"). Renaming a temp
/// file into place does not help (same inode, same open descriptor), so the
/// bytes are written by a short-lived `sh` child instead: once it has been
/// waited for, nobody holds the file open for writing.
fn fake_tool(dir: &Path, name: &str, script: &str) -> PathBuf {
    use std::io::Write;
    let path = dir.join(name);
    let mut writer = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("cat > \"$1\"")
        .arg("sh")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh to write the fake tool");
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(format!("#!/bin/sh\n{script}\n").as_bytes())
        .unwrap();
    assert!(
        writer.wait().unwrap().success(),
        "writing {}",
        path.display()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn input(dir: &Path) -> PathBuf {
    let p = dir.join("in put ü.pdf");
    std::fs::write(&p, b"%PDF-1.4 synthetic").unwrap();
    p
}

fn opts(path: Option<OsString>, fallback: Vec<PathBuf>, timeout: Duration) -> ConvertOptions {
    ConvertOptions {
        search_path: path,
        fallback_dirs: fallback,
        timeout,
    }
}

#[tokio::test]
async fn missing_converter_is_a_clear_error_not_a_hang() {
    let empty = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let o = opts(
        Some(empty.path().as_os_str().to_owned()),
        vec![],
        Duration::from_secs(5),
    );
    let started = Instant::now();
    let err = convert_doc_to_text_with(DocKind::Pdf, &input(work.path()), &o)
        .await
        .unwrap_err()
        .to_string();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(err.contains("pdftotext is not installed"), "{err}");
    assert!(err.contains("poppler"), "install hint missing: {err}");
    let err = convert_doc_to_text_with(DocKind::Docx, &input(work.path()), &o)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("pandoc is not installed"), "{err}");
}

#[tokio::test]
async fn launchd_style_minimal_path_still_finds_a_service_dir_tool() {
    // launchd starts jobs with PATH=/usr/bin:/bin:/usr/sbin:/sbin unless the
    // plist sets one; Homebrew tools live in /opt/homebrew/bin or
    // /usr/local/bin. The fallback dirs cover a plist without them.
    let brew = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fake_tool(brew.path(), "pdftotext", "echo \"converted $2\"");
    let minimal = tempfile::tempdir().unwrap();
    let o = opts(
        Some(minimal.path().as_os_str().to_owned()),
        vec![brew.path().to_path_buf()],
        Duration::from_secs(5),
    );
    assert_eq!(
        resolve_tool("pdftotext", &o).as_deref(),
        Some(brew.path().join("pdftotext").as_path())
    );
    let input = input(work.path());
    let text = convert_doc_to_text_with(DocKind::Pdf, &input, &o)
        .await
        .unwrap();
    assert_eq!(text.trim(), format!("converted {}", input.display()));
}

#[tokio::test]
async fn path_entries_win_over_fallback_dirs_and_non_executables_are_skipped() {
    let on_path = tempfile::tempdir().unwrap();
    let fallback = tempfile::tempdir().unwrap();
    let not_exec = on_path.path().join("pandoc");
    std::fs::write(&not_exec, "not a program").unwrap();
    std::fs::set_permissions(&not_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
    let real = fake_tool(fallback.path(), "pandoc", "echo ok");
    let o = opts(
        Some(on_path.path().as_os_str().to_owned()),
        vec![fallback.path().to_path_buf()],
        Duration::from_secs(5),
    );
    assert_eq!(resolve_tool("pandoc", &o), Some(real));
    let winner = fake_tool(on_path.path(), "pdftotext", "echo ok");
    fake_tool(fallback.path(), "pdftotext", "echo ok");
    assert_eq!(resolve_tool("pdftotext", &o), Some(winner));
}

#[tokio::test]
async fn a_stuck_converter_is_killed_after_the_timeout() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let marker = work.path().join("still-running");
    fake_tool(
        bin.path(),
        "pdftotext",
        &format!("sleep 5; touch '{}'", marker.display()),
    );
    let o = opts(
        Some(bin.path().as_os_str().to_owned()),
        vec![],
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let err = extract_text_with(DocKind::Pdf, &input(work.path()), None, &o)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(started.elapsed() < Duration::from_secs(3), "{msg}");
    assert!(msg.contains("timed out"), "{msg}");
    // The child was killed, not left running in the background.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(!marker.exists(), "timed-out converter kept running");
}

#[tokio::test]
async fn failing_converter_reports_exit_status_and_stderr() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fake_tool(bin.path(), "pandoc", "echo 'bad docx' >&2; exit 3");
    let o = opts(
        Some(bin.path().as_os_str().to_owned()),
        vec![],
        Duration::from_secs(5),
    );
    let err = convert_doc_to_text_with(DocKind::Docx, &input(work.path()), &o)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("pandoc exited") && err.contains("bad docx"),
        "{err}"
    );
}

/// ETXTBSY regression (#1334 class): fake tools written on some threads are
/// executed at once while other threads keep forking. With the tool written
/// through `std::fs::write` in this process this failed on Linux with "Text
/// file busy"; every exec must now succeed.
#[test]
fn fake_tools_run_while_other_threads_fork() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    // ETXTBSY is Linux behavior; elsewhere (macOS execs of fresh scripts are
    // slow) a short run keeps the helper covered.
    let (threads, rounds) = if cfg!(target_os = "linux") {
        (8, 25)
    } else {
        (4, 5)
    };
    let threads: Vec<_> = (0..threads)
        .map(|t| {
            let root = root.clone();
            std::thread::spawn(move || {
                for i in 0..rounds {
                    let tool = fake_tool(&root, &format!("tool-{t}-{i}"), "exit 0");
                    let out = std::process::Command::new(&tool)
                        .output()
                        .unwrap_or_else(|e| panic!("exec {}: {e}", tool.display()));
                    assert!(out.status.success());
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("no exec failed");
    }
}
