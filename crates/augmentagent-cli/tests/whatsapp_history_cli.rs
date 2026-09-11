use std::{fs, process::Command};

#[test]
fn whatsapp_archive_poll_is_opt_in_and_imports_plain_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let run = |bundle: Option<&std::path::Path>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        cmd.current_dir(tmp.path())
            .env("AUGMENTAGENT_DB", tmp.path().join("agent.db"))
            .env_remove("AUGMENTAGENT_WHATSAPP_HISTORY_DIR")
            .args(["whatsapp-history", "poll-once"]);
        if let Some(bundle) = bundle {
            cmd.env("AUGMENTAGENT_WHATSAPP_HISTORY_DIR", bundle);
        }
        cmd.output().unwrap()
    };
    assert!(!run(None).status.success());
    let bundle = tmp.path().join("bundle");
    fs::create_dir_all(bundle.join("conversations")).unwrap();
    fs::write(bundle.join("conversations/index.json"), "{}").unwrap();
    let output = run(Some(&bundle));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["inserted"], 0);
    assert_eq!(report["skipped"], 0);
}
