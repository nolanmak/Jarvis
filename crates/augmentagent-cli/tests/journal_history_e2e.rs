//! Synthetic recovery through the installed CLI surface and a local Git mirror.
use augmentagent_channel_journal::{client::Entry, section};
use std::process::Command;

#[test]
fn journal_history_survives_git_sync_and_exports_exact_old_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let wiki = temp.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let mut entry = Entry {
        id: "synthetic-party-plan".into(), owner_id: "synthetic-owner".into(),
        created_at: "2026-07-01T08:00:00.000Z".into(), content: None,
        title: Some("Synthetic invite list".into()), topic: Some("Journal".into()),
        bookmarked: None, updated_at: None, version: Some(1), deleted: Some(false),
        last_changed_at: None, owner: None,
    };
    let current = section::write_entry(&wiki, &entry, "Invite Guest A and Guest B.\n").unwrap();
    let original = std::fs::read(&current).unwrap();
    let revision = section::revisions(&wiki, &entry.id).unwrap().remove(0);
    entry.version = Some(2);
    section::write_entry(&wiki, &entry, "Replaced task list.\n").unwrap();
    std::fs::write(wiki.join(".gitignore"), "*.lock\n").unwrap();
    for args in [vec!["init", "--quiet"], vec!["add", "."],
                 vec!["-c", "user.name=Synthetic", "-c", "user.email=fixture@example.com", // pii-ok: synthetic
                      "commit", "--quiet", "-m", "Synthetic private mirror"]] {
        assert!(Command::new("git").args(args).current_dir(&wiki).status().unwrap().success());
    }
    let mirror = temp.path().join("mirror");
    assert!(Command::new("git").args(["clone", "--quiet"]).arg(&wiki).arg(&mirror)
        .status().unwrap().success());
    let before = std::fs::read(mirror.join(section::entry_rel_path(&entry))).unwrap();
    let invoke = |extra: &[&str]| Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db").arg(temp.path().join("synthetic.db"))
        .arg("--wiki-dir").arg(&mirror).args(["journal", "history", "--entry-id", &entry.id])
        .args(extra).current_dir(temp.path()).output().unwrap();
    let listed = invoke(&[]);
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    assert!(String::from_utf8_lossy(&listed.stdout).contains(&revision));
    let recovered = invoke(&["--revision", &revision]);
    assert!(recovered.status.success(), "{}", String::from_utf8_lossy(&recovered.stderr));
    assert_eq!(recovered.stdout, original);
    assert_eq!(std::fs::read(mirror.join(section::entry_rel_path(&entry))).unwrap(), before);
    assert!(!invoke(&["--revision", "../escape"]).status.success());
}
