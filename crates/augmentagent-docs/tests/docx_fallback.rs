//! #1385 — a `.docx` attachment must still yield readable text on a host
//! without pandoc. The fallback unzips `word/document.xml` through `python3`
//! (stdlib only), so the common case works with no extra binary.
//!
//! Hermeticity rule for every "pandoc is missing" test here: the *pandoc*
//! lookup (`search_path` + `fallback_dirs`) only ever sees empty temp dirs,
//! while the *fallback* lookup (`fallback_search_path` + `fallback_tool_dirs`)
//! points at a separate temp dir holding nothing but a `python3` symlink.
//! The two sets are disjoint, so a host with an apt-installed pandoc in
//! /usr/bin cannot resolve it and quietly invalidate the premise.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use augmentagent_docs::{convert_doc_to_text_with, resolve_tool, ConvertOptions, DocKind};

/// See `converter_bounds.rs`: write executables via a short-lived `sh` child
/// so this process never holds a write descriptor (ETXTBSY, #1334).
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

/// Absolute path of the real interpreter, resolved against the process PATH
/// (never used as a *directory* — see the module note).
fn real_python3() -> PathBuf {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v python3")
        .output()
        .expect("look up python3");
    assert!(out.status.success(), "python3 is required for these tests");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// A fresh dir containing only a `python3` symlink to the real interpreter.
fn python_only_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(real_python3(), dir.path().join("python3")).unwrap();
    dir
}

/// pandoc is provably unresolvable; the fallback sees `python_dir` only.
fn no_pandoc_opts(empty: &Path, python_dir: &Path, timeout: Duration) -> ConvertOptions {
    ConvertOptions {
        search_path: Some(empty.as_os_str().to_owned()),
        fallback_dirs: vec![],
        fallback_search_path: Some(python_dir.as_os_str().to_owned()),
        fallback_tool_dirs: vec![],
        timeout,
    }
}

/// A real two-paragraph .docx, zipped by python3's stdlib.
fn two_paragraph_docx(dir: &Path, one: &str, two: &str) -> PathBuf {
    let path = dir.join("re vised ü.docx");
    let script = r#"
import sys, zipfile
path, one, two = sys.argv[1], sys.argv[2], sys.argv[3]
NS = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
body = '<?xml version="1.0" encoding="UTF-8"?>' \
       '<w:document xmlns:w="%s"><w:body>' \
       '<w:p><w:r><w:t>%s</w:t></w:r></w:p>' \
       '<w:p><w:r><w:t>%s</w:t></w:r></w:p>' \
       '</w:body></w:document>' % (NS, one, two)
with zipfile.ZipFile(path, 'w') as z:
    z.writestr('[Content_Types].xml', '<?xml version="1.0"?><Types/>')
    z.writestr('word/document.xml', body)
"#;
    let out = std::process::Command::new(real_python3())
        .arg("-c")
        .arg(script)
        .arg(&path)
        .arg(one)
        .arg(two)
        .output()
        .expect("build the docx");
    assert!(
        out.status.success(),
        "building the docx: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

#[tokio::test]
async fn docx_extracts_without_pandoc_and_keeps_paragraph_breaks() {
    let empty = tempfile::tempdir().unwrap();
    let python = python_only_dir();
    let work = tempfile::tempdir().unwrap();
    let o = no_pandoc_opts(empty.path(), python.path(), Duration::from_secs(10));
    assert_eq!(
        resolve_tool("pandoc", &o),
        None,
        "premise: pandoc must be absent"
    );

    let doc = two_paragraph_docx(
        work.path(),
        "Clause one as revised.",
        "Clause two unchanged.",
    );
    let text = convert_doc_to_text_with(DocKind::Docx, &doc, &o)
        .await
        .expect("fallback extraction");
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        lines,
        vec!["Clause one as revised.", "Clause two unchanged."],
        "{text:?}"
    );
}

#[tokio::test]
async fn pandoc_wins_when_present() {
    let bin = tempfile::tempdir().unwrap();
    let python = python_only_dir();
    let work = tempfile::tempdir().unwrap();
    fake_tool(bin.path(), "pandoc", "echo PANDOC-SENTINEL");
    let o = ConvertOptions {
        search_path: Some(bin.path().as_os_str().to_owned()),
        fallback_dirs: vec![],
        fallback_search_path: Some(python.path().as_os_str().to_owned()),
        fallback_tool_dirs: vec![],
        timeout: Duration::from_secs(10),
    };
    let doc = two_paragraph_docx(work.path(), "Clause one.", "Clause two.");
    let text = convert_doc_to_text_with(DocKind::Docx, &doc, &o)
        .await
        .unwrap();
    assert_eq!(text.trim(), "PANDOC-SENTINEL");
    assert!(
        !text.contains("Clause one."),
        "fallback ran anyway: {text:?}"
    );
}

#[tokio::test]
async fn legacy_doc_without_pandoc_blames_the_operator_not_the_caller() {
    let empty = tempfile::tempdir().unwrap();
    let python = python_only_dir();
    let work = tempfile::tempdir().unwrap();
    let legacy = work.path().join("contract.doc");
    std::fs::write(&legacy, b"\xd0\xcf\x11\xe0 legacy ole2").unwrap();
    let o = no_pandoc_opts(empty.path(), python.path(), Duration::from_secs(10));
    let err = convert_doc_to_text_with(DocKind::Doc, &legacy, &o)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("pandoc is not installed"), "{err}");
    assert!(err.contains("operator must install"), "{err}");
    assert!(!err.contains("brew install"), "{err}");
    assert!(!err.contains("apt install"), "{err}");
    // No fallback is claimed for the legacy format.
    assert!(!err.contains("word/document.xml"), "{err}");
}

#[tokio::test]
async fn a_corrupt_docx_without_pandoc_names_both_failures() {
    let empty = tempfile::tempdir().unwrap();
    let python = python_only_dir();
    let work = tempfile::tempdir().unwrap();
    let bogus = work.path().join("renamed.docx");
    std::fs::write(&bogus, vec![0x42u8; 64]).unwrap();
    let o = no_pandoc_opts(empty.path(), python.path(), Duration::from_secs(10));
    let err = convert_doc_to_text_with(DocKind::Docx, &bogus, &o)
        .await
        .expect_err("a non-zip .docx must error, not come back empty")
        .to_string();
    assert!(err.contains("pandoc is not installed"), "{err}");
    assert!(err.contains("word/document.xml"), "{err}");
    assert!(!err.contains("Traceback"), "python traceback leaked: {err}");
}

#[tokio::test]
async fn both_missing_names_pandoc_and_python3() {
    let empty = tempfile::tempdir().unwrap();
    let no_python = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let o = no_pandoc_opts(empty.path(), no_python.path(), Duration::from_secs(10));
    let doc = two_paragraph_docx(work.path(), "One.", "Two.");
    let err = convert_doc_to_text_with(DocKind::Docx, &doc, &o)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("pandoc is not installed"), "{err}");
    assert!(err.contains("python3 is not installed"), "{err}");
    assert!(err.contains("operator must install"), "{err}");
}

#[tokio::test]
async fn fallback_respects_the_timeout() {
    let empty = tempfile::tempdir().unwrap();
    let slow = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fake_tool(slow.path(), "python3", "sleep 5");
    let o = no_pandoc_opts(empty.path(), slow.path(), Duration::from_millis(300));
    let doc = two_paragraph_docx(work.path(), "One.", "Two.");
    let started = Instant::now();
    let err = convert_doc_to_text_with(DocKind::Docx, &doc, &o)
        .await
        .unwrap_err()
        .to_string();
    assert!(started.elapsed() < Duration::from_secs(3), "{err}");
    assert!(err.contains("timed out"), "{err}");
}
