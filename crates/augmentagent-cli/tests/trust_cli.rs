//! #1833 C1 — `augmentagent trust …` against a temp config, temp store and
//! temp wiki. Example data only.
use std::path::Path;
use std::process::{Command, Stdio};

fn cli(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    c.env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("AUGMENTAGENT_TRUSTED_CONTACTS", home.join("config/trusted-contacts.toml"))
        .current_dir(home)
        .stdin(Stdio::null());
    c.arg("--db").arg(home.join("data.db"));
    c
}

fn wiki(home: &Path) -> std::path::PathBuf {
    let people = home.join("wiki/people");
    std::fs::create_dir_all(&people).unwrap();
    std::fs::write(
        people.join("jane-doe.md"),
        "---\nkind: person\nidentities:\n  email: [\"Jane@Work.Example.Test\"]\n  imessage: [\"(202) 555-0100\"]\n---\n# Jane\n",
    )
    .unwrap();
    home.join("wiki")
}

fn run(c: &mut Command) -> (bool, String, String) {
    let out = c.output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn trust_add_refuses_non_tty() {
    let home = tempfile::tempdir().unwrap();
    let wiki = wiki(home.path());
    for args in [
        vec!["trust", "add", "jane-doe", "--tier", "actions"],
        vec!["trust", "sync", "jane-doe"],
    ] {
        let (ok, _out, err) = run(cli(home.path()).arg("--wiki-dir").arg(&wiki).args(&args));
        assert!(!ok, "{args:?} must refuse without a TTY");
        assert!(err.to_lowercase().contains("tty"), "{args:?}: {err}");
    }
    // Even piped "y" answers are refused: stdin is not a terminal.
    let mut c = cli(home.path());
    c.arg("--wiki-dir").arg(&wiki).args(["trust", "add", "jane-doe", "--tier", "actions"]);
    c.stdin(Stdio::piped());
    let mut child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    {
        use std::io::Write;
        let _ = child.stdin.take().unwrap().write_all(b"y\ny\ny\ny\n");
    }
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(!home.path().join("config/trusted-contacts.toml").exists(), "nothing written");
}

#[test]
fn explicit_add_confirm_pause_remove_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let (ok, out, err) = run(cli(h).args([
        "trust", "add", "jane-doe", "--tier", "actions",
        "--handle", "imessage:+12025550100",
        "--handle", "email:jane@work.example.test",
        "--calendar-email", "jane@work.example.test",
        "--allow", "calendar.add_guest", "--allow", "calendar.send_details",
    ]));
    assert!(ok, "{err}");
    assert!(out.contains("pending"), "{out}");
    let text = std::fs::read_to_string(h.join("config/trusted-contacts.toml")).unwrap();
    assert!(text.contains("imessage:+12025550100"), "{text}");

    // Non-canonical handles are refused and nothing changes.
    let (ok, _, err) = run(cli(h).args(["trust", "add", "sam-roe", "--tier", "full", "--handle", "email:Sam@Example.Test"]));
    assert!(!ok);
    assert!(err.contains("canonical"), "{err}");
    // Duplicate slug refused.
    let (ok, _, _) = run(cli(h).args(["trust", "add", "jane-doe", "--tier", "full", "--handle", "email:x@example.test"]));
    assert!(!ok);
    // A handle already on another contact is refused.
    let (ok, _, err) = run(cli(h).args(["trust", "add", "sam-roe", "--tier", "full", "--handle", "imessage:+12025550100"]));
    assert!(!ok, "duplicate handle must be refused");
    assert!(err.contains("imessage:+12025550100"), "{err}");

    let (ok, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["contacts"][0]["slug"], "jane-doe");
    assert_eq!(v["contacts"][0]["state"], "pending");
    assert_eq!(v["global_pause"], false);

    let (ok, _, err) = run(cli(h).args(["trust", "confirm", "jane-doe"]));
    assert!(ok, "{err}");
    let (ok, _, _) = run(cli(h).args(["trust", "confirm", "nobody"]));
    assert!(!ok);
    let (_, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["contacts"][0]["state"], "active");

    assert!(run(cli(h).args(["trust", "pause", "jane-doe"])).0);
    assert!(run(cli(h).args(["trust", "pause-all"])).0);
    let (_, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["contacts"][0]["paused"], true);
    assert_eq!(v["global_pause"], true);
    assert!(run(cli(h).args(["trust", "resume", "jane-doe"])).0);
    assert!(run(cli(h).args(["trust", "resume-all"])).0);
    let (_, out, _) = run(cli(h).args(["trust", "list"]));
    assert!(out.contains("jane-doe") && out.contains("active"), "{out}");

    assert!(run(cli(h).args(["trust", "remove", "jane-doe"])).0);
    let (_, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["contacts"].as_array().unwrap().len(), 0);
    // Re-adding starts pending again: removal forgot the confirmation.
    assert!(run(cli(h).args(["trust", "add", "jane-doe", "--tier", "full", "--handle", "email:jane@work.example.test"])).0);
    let (_, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["contacts"][0]["state"], "pending");
}

#[test]
fn list_reports_disabled_contacts_and_parse_errors() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    std::fs::create_dir_all(h.join("config")).unwrap();
    std::fs::write(
        h.join("config/trusted-contacts.toml"),
        "[[contact]]\nwiki = \"bad-one\"\nhandles = [\"email:b@example.test\"]\n",
    )
    .unwrap();
    let (ok, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["contacts"].as_array().unwrap().len(), 0);
    assert!(v["problems"][0].as_str().unwrap().contains("bad-one"), "{v}");

    std::fs::write(h.join("config/trusted-contacts.toml"), "[[contact]\n").unwrap();
    let (ok, out, _) = run(cli(h).args(["trust", "list", "--json"]));
    assert!(ok);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["fatal"].is_string(), "{v}");
    // A broken file is never overwritten by the CLI.
    let (ok, _, err) = run(cli(h).args(["trust", "add", "x", "--tier", "full", "--handle", "email:x@example.test"]));
    assert!(!ok, "must refuse to rewrite an unparseable file");
    assert!(err.contains("by hand"), "{err}");
}
