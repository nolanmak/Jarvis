//! Apple Notes bundle → searchable history (#1058, #1059). Fixture bundles
//! are written the way `scripts/apple-notes/` writes them; one test runs the
//! real exporter over a synthetic database to pin Python↔Rust compatibility.
use augmentagent_channel_apple_notes::{
    capture_email, content_hash, poll_once, read_note, Config, NoteDoc,
};
use augmentagent_mcp_memory::Server;
use augmentagent_store::Store;
use serde_json::json;
use std::{fs, path::Path, process::Command};

const UUID_A: &str = "AAAAAAAA-0000-0000-0000-000000000001";
const UUID_B: &str = "BBBBBBBB-0000-0000-0000-000000000002";

fn write_note(root: &Path, rel: &str, title: &str, uuid: &str, modified: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            "---\ntype: \"Apple Note\"\nidentifier: \"{uuid}\"\ntitle: {}\nfolder: \"Notes\"\naccount: \"iCloud\"\ncreated: \"2026-09-01T10:00:00-04:00\"\nmodified: \"{modified}\"\n---\n\n{body}\n",
            serde_json::to_string(title).unwrap()
        ),
    )
    .unwrap();
}

fn index(root: &Path, entries: serde_json::Value) {
    fs::create_dir_all(root.join("notes")).unwrap();
    fs::write(root.join("notes/index.json"), entries.to_string()).unwrap();
}

fn bundle(root: &Path) {
    write_note(
        root,
        "notes/notes/cabin-plan.md",
        "Cabin plan",
        UUID_A,
        "2026-09-02T10:00:00-04:00",
        "Book the cabin for October.\nBring firewood.",
    );
    write_note(
        root,
        "notes/notes/groceries.md",
        "Groceries",
        UUID_B,
        "2026-09-03T10:00:00-04:00",
        "eggs\nmilk",
    );
    index(
        root,
        json!({
            UUID_A: {"title": "Cabin plan", "folder": "Notes", "path": "notes/notes/cabin-plan.md",
                     "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"},
            UUID_B: {"title": "Groceries", "folder": "Notes", "path": "notes/notes/groceries.md",
                     "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-03T10:00:00-04:00"},
        }),
    );
}

#[test]
fn read_note_splits_frontmatter_once_and_keeps_body_rules() {
    let tmp = tempfile::tempdir().unwrap();
    write_note(
        tmp.path(),
        "notes/notes/x.md",
        "Weird: \"title\" #1",
        UUID_A,
        "2026-09-02T10:00:00-04:00",
        "body\n---\nnot frontmatter",
    );
    let doc: NoteDoc = read_note(&tmp.path().join("notes/notes/x.md")).unwrap();
    assert_eq!(doc.title, "Weird: \"title\" #1");
    assert_eq!(doc.identifier, UUID_A);
    assert_eq!(doc.text, "body\n---\nnot frontmatter\n");
    assert_eq!(doc.folder, "Notes");
    assert!(doc.redactions.is_empty());
}

#[test]
fn read_note_reports_redactions_and_attachments() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("notes/notes/x.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "---\ntype: \"Apple Note\"\nidentifier: \"ID\"\ntitle: \"Creds\"\nfolder: \"Work\"\naccount: \"iCloud\"\ncreated: \"2026-09-01T10:00:00-04:00\"\nmodified: \"2026-09-02T10:00:00-04:00\"\nattachments:\n  - \"image/jpeg IMG_1.jpeg\"\nredactions:\n  - \"aws-access-key\"\n---\n\nkey [REDACTED:aws-access-key]\n[attachment: image/jpeg IMG_1.jpeg]\n").unwrap();
    let doc = read_note(&path).unwrap();
    assert_eq!(doc.redactions, vec!["aws-access-key"]);
    assert_eq!(doc.attachments, vec!["image/jpeg IMG_1.jpeg"]);
    assert!(doc.text.contains("[REDACTED:aws-access-key]"));
}

#[test]
fn content_hash_ignores_modified_and_tracks_text() {
    let tmp = tempfile::tempdir().unwrap();
    write_note(
        tmp.path(),
        "a.md",
        "T",
        UUID_A,
        "2026-09-02T10:00:00-04:00",
        "body",
    );
    write_note(
        tmp.path(),
        "b.md",
        "T",
        UUID_A,
        "2026-09-09T10:00:00-04:00",
        "body",
    );
    write_note(
        tmp.path(),
        "c.md",
        "T",
        UUID_A,
        "2026-09-02T10:00:00-04:00",
        "bodY",
    );
    let h = |n: &str| content_hash(&read_note(&tmp.path().join(n)).unwrap());
    assert_eq!(h("a.md"), h("b.md"));
    assert_ne!(h("a.md"), h("c.md"));
}

#[test]
fn backfill_is_searchable_and_repeat_safe() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let db = tmp.path().join("data.db");
    let store = Store::open(&db).unwrap();
    let first = poll_once(tmp.path(), &store).unwrap();
    assert_eq!(
        (first.new, first.updated, first.deleted, first.unchanged),
        (2, 0, 0, 0)
    );
    assert!(first.first_run);
    let again = poll_once(tmp.path(), &store).unwrap();
    assert_eq!((again.new, again.unchanged), (0, 2));
    assert!(!again.first_run);
    assert!(again.deltas.is_empty());

    let memory = Server::open(&db).unwrap();
    let hits = memory
        .search_conversation_history(Some("firewood"), None, None, Some("apple_notes"), None)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].thread_id.as_deref(),
        Some(&*format!("apple-notes:{UUID_A}"))
    );
    assert_eq!(hits[0].timestamp_ms, 1788357600000); // 2026-09-02T10:00:00-04:00
    assert!(
        hits[0].snippet.contains("Cabin plan"),
        "title must be visible in results"
    );
}

#[test]
fn edit_replaces_the_row_and_search_sees_only_the_latest_text() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let db = tmp.path().join("data.db");
    let store = Store::open(&db).unwrap();
    poll_once(tmp.path(), &store).unwrap();
    write_note(
        tmp.path(),
        "notes/notes/cabin-plan.md",
        "Cabin plan",
        UUID_A,
        "2026-09-05T10:00:00-04:00",
        "Book the cabin for November.\nBring firewood.",
    );
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!((report.new, report.updated, report.unchanged), (0, 1, 1));
    assert_eq!(report.deltas.len(), 1);
    assert!(!report.first_run);
    let memory = Server::open(&db).unwrap();
    assert_eq!(
        memory
            .search_conversation_history(Some("October"), None, None, Some("apple_notes"), None)
            .unwrap()
            .len(),
        0
    );
    let hits = memory
        .search_conversation_history(Some("November"), None, None, Some("apple_notes"), None)
        .unwrap();
    assert_eq!(hits.len(), 1);
    let capture = capture_email(&report.deltas[0]);
    assert_eq!(capture.platform, "apple_notes");
    assert!(
        capture.subject.contains("edited"),
        "capture must say the note changed: {}",
        capture.subject
    );
    assert!(capture.body.contains("November"));
}

#[test]
fn modified_only_bump_produces_no_delta() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let store = Store::open(tmp.path().join("data.db")).unwrap();
    poll_once(tmp.path(), &store).unwrap();
    write_note(
        tmp.path(),
        "notes/notes/groceries.md",
        "Groceries",
        UUID_B,
        "2026-09-09T10:00:00-04:00",
        "eggs\nmilk",
    );
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!((report.updated, report.unchanged), (0, 2));
    assert!(report.deltas.is_empty());
}

#[test]
fn tombstone_removes_the_row_and_state() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let db = tmp.path().join("data.db");
    let store = Store::open(&db).unwrap();
    poll_once(tmp.path(), &store).unwrap();
    fs::remove_file(tmp.path().join("notes/notes/groceries.md")).unwrap();
    index(
        tmp.path(),
        json!({
            UUID_A: {"title": "Cabin plan", "folder": "Notes", "path": "notes/notes/cabin-plan.md",
                     "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"},
            UUID_B: {"title": "Groceries", "folder": "Notes", "created": "2026-09-01T10:00:00-04:00",
                     "modified": "2026-09-03T10:00:00-04:00", "deleted": "2026-09-10T10:00:00-04:00"},
        }),
    );
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!((report.deleted, report.unchanged), (1, 1));
    let memory = Server::open(&db).unwrap();
    assert!(memory
        .search_conversation_history(Some("milk"), None, None, Some("apple_notes"), None)
        .unwrap()
        .is_empty());
    // A second pass over the same tombstone is quiet.
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!((report.deleted, report.unchanged), (0, 1));
    // A note absent from the index entirely (tombstone pruned) is also retired.
    index(
        tmp.path(),
        json!({
            UUID_A: {"title": "Cabin plan", "folder": "Notes", "path": "notes/notes/cabin-plan.md",
                     "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"},
        }),
    );
    assert_eq!(poll_once(tmp.path(), &store).unwrap().deleted, 0);
}

#[test]
fn unreadable_or_escaping_paths_do_not_block_good_notes() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    index(
        tmp.path(),
        json!({
            UUID_A: {"title": "Cabin plan", "folder": "Notes", "path": "notes/notes/cabin-plan.md",
                     "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"},
            "bad": {"title": "Bad", "folder": "x", "path": "../../outside.md",
                    "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"},
            "missing": {"title": "Missing", "folder": "x", "path": "notes/notes/missing.md",
                        "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"},
        }),
    );
    let report = poll_once(tmp.path(), &Store::open(tmp.path().join("db")).unwrap()).unwrap();
    assert_eq!((report.new, report.skipped), (1, 2));
}

#[test]
fn malformed_index_is_an_error_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    index(tmp.path(), json!([1, 2]));
    let err = poll_once(tmp.path(), &Store::open(tmp.path().join("db")).unwrap()).unwrap_err();
    assert!(format!("{err:#}").contains("index.json"), "{err:#}");
}

#[test]
fn configuration_is_opt_in_and_rejects_missing_directories() {
    assert!(Config::from_path(None).unwrap().is_none());
    assert!(Config::from_path(Some(" ")).unwrap().is_none());
    let tmp = tempfile::tempdir().unwrap();
    assert!(Config::from_path(Some(tmp.path().to_str().unwrap()))
        .unwrap()
        .is_some());
    assert!(Config::from_path(Some(tmp.path().join("missing").to_str().unwrap())).is_err());
}

#[tokio::test]
async fn git_feed_pull_imports_edited_notes() {
    use augmentagent_channel_apple_notes::refresh;
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("upstream");
    bundle(&upstream);
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "-c",
                "commit.gpgsign=false"
            ])
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "initial"]);
    let checkout = tmp.path().join("checkout");
    assert!(Command::new("git")
        .args(["clone", "--quiet"])
        .arg(&upstream)
        .arg(&checkout)
        .status()
        .unwrap()
        .success());
    let store = Store::open(tmp.path().join("db")).unwrap();
    assert_eq!(poll_once(&checkout, &store).unwrap().new, 2);
    write_note(
        &upstream,
        "notes/notes/groceries.md",
        "Groceries",
        UUID_B,
        "2026-09-09T10:00:00-04:00",
        "eggs\nmilk\nbread",
    );
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "edit"]);
    let config = Config::from_path(Some(checkout.to_str().unwrap()))
        .unwrap()
        .unwrap();
    assert!(refresh(&config).await.unwrap());
    let report = poll_once(&checkout, &store).unwrap();
    assert_eq!(report.updated, 1);
    assert!(capture_email(&report.deltas[0]).body.contains("bread"));
}

#[test]
fn actual_python_exporter_produces_readable_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = r#"
import sys
from pathlib import Path
root, dest = map(Path, sys.argv[1:])
sys.path.insert(0, str(root/'scripts/apple-notes'))
sys.path.insert(0, str(root/'scripts/apple-notes/tests'))
from test_sync import make_fixture_db, add_note, add_attachment
from apple_notes_sync import sync
con = make_fixture_db(dest/'NoteStore.sqlite')
add_note(con, 10, 'Compat: "quoted" #1', 'Compat: "quoted" #1\ncabin compatibility test\n￼\nsecret AKIAIOSFODNN7EXAMPLE', attachments=[('ATT-1', 'public.jpeg')])  # pii-ok gitleaks:allow synthetic fixture
add_attachment(con, 10, 'ATT-1', 'public.jpeg', 'IMG_0001.jpeg')
sync(dest/'NoteStore.sqlite', dest, dest/'.sync_state.json')
con.close()
"#;
    assert!(Command::new("python3")
        .args(["-c", script])
        .arg(root)
        .arg(tmp.path())
        .status()
        .unwrap()
        .success());
    let store = Store::open(tmp.path().join("agent.db")).unwrap();
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!(report.new, 1);
    let email = capture_email(&report.deltas[0]);
    assert_eq!(email.platform, "apple_notes");
    assert_eq!(email.kind, "note");
    assert!(
        email.subject.contains("Compat: \"quoted\" #1"),
        "{}",
        email.subject
    );
    assert!(email
        .body
        .contains("[attachment: image/jpeg IMG_0001.jpeg]"));
    assert!(email.body.contains("[REDACTED:aws-access-key]"));
    assert!(!email.body.contains("AKIA"));
    assert_eq!(email.attachments, vec!["image/jpeg IMG_0001.jpeg"]);
}

#[test]
fn dry_run_reports_against_real_state_and_writes_nothing() {
    use augmentagent_channel_apple_notes::poll;
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let db = tmp.path().join("data.db");
    let store = Store::open(&db).unwrap();
    poll_once(tmp.path(), &store).unwrap();
    write_note(
        tmp.path(),
        "notes/notes/groceries.md",
        "Groceries",
        UUID_B,
        "2026-09-09T10:00:00-04:00",
        "eggs\nmilk\nbread",
    );
    let preview = poll(tmp.path(), &store, true).unwrap();
    assert_eq!((preview.new, preview.updated, preview.unchanged), (0, 1, 1));
    assert!(!preview.first_run);
    assert_eq!(preview.deltas.len(), 1);
    // Nothing persisted: the real run still sees the edit, and search still has the old text.
    let memory = Server::open(&db).unwrap();
    assert!(memory
        .search_conversation_history(Some("bread"), None, None, Some("apple_notes"), None)
        .unwrap()
        .is_empty());
    assert_eq!(poll_once(tmp.path(), &store).unwrap().updated, 1);
}

#[test]
fn capture_fan_out_skips_first_run_and_disabled_capture() {
    use augmentagent_channel_apple_notes::capture_emails;
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let store = Store::open(tmp.path().join("data.db")).unwrap();
    let first = poll_once(tmp.path(), &store).unwrap();
    assert!(first.first_run && first.deltas.len() == 2);
    assert!(
        capture_emails(&first, true).is_empty(),
        "full-history pass must not fan out"
    );
    write_note(
        tmp.path(),
        "notes/notes/groceries.md",
        "Groceries",
        UUID_B,
        "2026-09-09T10:00:00-04:00",
        "eggs\nmilk\nbread",
    );
    let second = poll_once(tmp.path(), &store).unwrap();
    assert!(capture_emails(&second, false).is_empty(), "capture off");
    let emails = capture_emails(&second, true);
    assert_eq!(emails.len(), 1);
    assert!(emails[0]
        .subject
        .starts_with("Apple Note (edited): Groceries"));
}
