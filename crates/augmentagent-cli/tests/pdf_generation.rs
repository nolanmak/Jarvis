//! #992: real CLI, converter and Discord attachment preparation; no network.
use augmentagent_approval_discord::attachments::prepare_answer_delivery;
use std::process::{Command, Output};

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .env("WIKI_ROOT", root)
        .current_dir(root)
        .args(["doc", "render-pdf"])
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test]
async fn pdf_cli_renders_and_prepares_discord_attachment_without_database() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("deliverables")).unwrap();
    std::fs::write(
        dir.path().join("deliverables/packet.md"),
        "# Lawyer packet\n\nCafé evidence — final exhibit.",
    )
    .unwrap();
    let out = run(
        dir.path(),
        &[
            "deliverables/packet.md",
            "--out",
            "deliverables/packet.pdf",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["attach"], "ATTACH: deliverables/packet.pdf");
    let pdf = std::fs::read(dir.path().join("deliverables/packet.pdf")).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    assert_eq!(receipt["bytes"], pdf.len());
    assert!(!dir.path().join("data.db").exists());
    let (text, files) = prepare_answer_delivery(
        &format!("Your packet.\n{}", receipt["attach"].as_str().unwrap()),
        Some(dir.path()),
    )
    .await;
    assert_eq!(text, "Your packet.");
    assert_eq!(files.len(), 1);
}

#[test]
fn pdf_cli_rejects_escape_empty_overwrite_and_wrong_extension() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source.md"), "# Document").unwrap();
    std::fs::write(root.path().join("empty.md"), "  ").unwrap();
    std::fs::write(root.path().join("existing.pdf"), "keep me").unwrap();
    std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    for args in [
        vec!["source.md", "--out", "../escape.pdf"],
        vec!["escape/secret.md", "--out", "secret.pdf"],
        vec!["source.md", "--out", "escape/escaped.pdf"],
        vec!["empty.md", "--out", "empty.pdf"],
        vec!["source.md", "--out", "existing.pdf"],
        vec!["source.md", "--out", "wrong.md"],
    ] {
        let out = run(root.path(), &args);
        assert!(!out.status.success(), "unexpected success: {args:?}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("ATTACH:"));
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("existing.pdf")).unwrap(),
        "keep me"
    );
    assert!(!outside.path().join("escaped.pdf").exists());
    assert!(!root.path().join("empty.pdf").exists());
}

#[test]
fn pdf_cli_requires_root_and_bounds_input() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("large.md"), "x".repeat(1024 * 1024 + 1)).unwrap();
    let out = run(root.path(), &["large.md"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exceeds 1 MiB"));
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .env_remove("WIKI_ROOT")
        .current_dir(root.path())
        .args(["doc", "render-pdf", "large.md"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("WIKI_ROOT must be set"));
}

#[cfg(unix)]
#[test]
fn pdf_cli_never_publishes_failed_invalid_or_oversized_renderer_output() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source.md"), "# Document").unwrap();
    let worker = bin.path().join("python3");
    for script in [
        "#!/bin/sh\n/bin/cat >/dev/null\necho intentional_failure >&2\nexit 1\n",
        "#!/bin/sh\n/bin/cat >/dev/null\nprintf 'not a PDF'\n",
        "#!/bin/sh\n/bin/cat >/dev/null\n/usr/bin/head -c 8388609 /dev/zero\n",
    ] {
        std::fs::write(&worker, script).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
            .env("WIKI_ROOT", root.path())
            .env("PATH", bin.path())
            .current_dir(root.path())
            .args(["doc", "render-pdf", "source.md"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!root.path().join("source.pdf").exists());
        assert!(out.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn pdf_cli_preserves_dependency_error_when_worker_exits_before_reading_input() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("source.md"), "Paragraph. ".repeat(50_000)).unwrap();
    let worker = bin.path().join("python3");
    std::fs::write(
        &worker,
        "#!/bin/sh\necho 'No module named reportlab' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .env("WIKI_ROOT", root.path())
        .env("PATH", bin.path())
        .current_dir(root.path())
        .args(["doc", "render-pdf", "source.md"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("No module named reportlab"), "{error}");
    assert!(!root.path().join("source.pdf").exists());
}
