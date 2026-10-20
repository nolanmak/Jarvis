//! #1451 — the contact-provenance gate on the real CLI mail-writing paths.
//!
//! The incident: a draft listing team contacts carried a mobile number
//! attributed to the owner — a stored third party's — and reached the
//! approval card. These tests reproduce that invocation exactly, with **no
//! `--wiki-dir`** (a top-level flag the drafting agent cannot pass: its Bash
//! allowlist requires `gmail` first), and assert the write is refused before
//! any draft, card or action row exists.

use std::process::{Command, Output};

use augmentagent_store::{PhoneIdentity, Store};
use rusqlite::Connection;
use tempfile::TempDir;

/// Same legacy-table preamble the other CLI tests use: `Store::open` runs
/// `ALTER TABLE` migrations and aborts on a fresh empty sqlite.
const LEGACY_SCHEMA_PREAMBLE: &str = r#"
    CREATE TABLE actions (id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
        fromEmail TEXT NOT NULL, subject TEXT NOT NULL, originalBody TEXT, draftBody TEXT,
        status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
        createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL);
    CREATE TABLE emails (messageId TEXT PRIMARY KEY, threadId TEXT, fromEmail TEXT NOT NULL,
        subject TEXT NOT NULL, body TEXT, receivedAt TEXT, accountEntityId TEXT,
        firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER,
        platform TEXT NOT NULL DEFAULT 'gmail', kind TEXT NOT NULL DEFAULT 'dm');
    CREATE TABLE gmail_accounts (id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
        label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
        createdAt INTEGER NOT NULL);
"#;

/// A number nobody can dial, stored in one formatting and written in
/// another, so the test also proves normalization happens on both sides.
const STORED_E164: &str = "+14155550132";
const BODY_RENDERING: &str = "(415) 555-0132";

struct Fixture {
    tmp: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let conn = Connection::open(tmp.path().join("data.db")).expect("open seed db");
        conn.execute_batch(LEGACY_SCHEMA_PREAMBLE)
            .expect("seed legacy schema");
        conn.execute(
            "INSERT INTO gmail_accounts (id, connectionId, email, entityId, active, createdAt) \
             VALUES ('g1', 'c1', 'owner@example.com', 'ent-1', 1, 0)",
            [],
        )
        .expect("seed account");
        drop(conn);
        Self { tmp }
    }

    /// Index the number under a stored person, the way CRM ingestion does.
    fn with_stored_phone(self) -> Self {
        Store::open(self.tmp.path().join("data.db"))
            .expect("open store")
            .upsert_phone_identity(&PhoneIdentity {
                phone: STORED_E164.into(),
                person_slug: "carla-example".into(),
                display_name: Some("Carla Example".into()),
                source: "carddav".into(),
            })
            .expect("seed phone identity");
        self
    }

    /// A hand-edited wiki person page — never ingested, so its `phone:` is
    /// absent from `identity_phone`. An empty `phone` leaves the page as a
    /// pure slug→email mapping, the only thing that can tell the gate a
    /// store-indexed number belongs to a recipient.
    fn with_wiki_page(self, emails: &str, phone: &str) -> Self {
        let people = self.tmp.path().join("wiki/people");
        std::fs::create_dir_all(&people).expect("people dir");
        std::fs::write(
            people.join("carla-example.md"),
            format!(
                "---\nkind: person\nidentities:\n  email: [{emails}]\n  \
                 phone: [\"{phone}\"]\n---\n\n# Carla Example\n"
            ),
        )
        .expect("write person page");
        self
    }

    /// The incident's invocation — a contact-list body — with no
    /// `--wiki-dir` and no inherited configuration.
    fn run(&self, wiki_root_env: bool, op: &[&str], body: &str, extra: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
        cmd.current_dir(self.tmp.path())
            .env_remove("COMPOSIO_API_KEY")
            .env_remove("WIKI_ROOT")
            .env("AUGMENTAGENT_DB", self.tmp.path().join("data.db"))
            .env("AUGMENTAGENT_DEFAULT_REGION", "US");
        if wiki_root_env {
            cmd.env("WIKI_ROOT", self.tmp.path().join("wiki"));
        }
        let envelope = ["--to", "client@example.org", "--subject", "Team contacts"];
        cmd.args(["gmail"])
            .args(op)
            .args(envelope)
            .args(["--body", body])
            .args(extra)
            .output()
            .expect("run cli")
    }

    fn action_count(&self) -> i64 {
        let conn = Connection::open(self.tmp.path().join("data.db")).expect("open db");
        conn.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0))
            .expect("count actions")
    }
}

/// Posted for approval, the way the drafting agent invokes it.
const COMPOSE: [&str; 2] = ["compose", "--post"];

/// `stored` — whether a record claims the number, i.e. whether the refusal
/// is expected to name its holder.
fn assert_refused(out: &Output, stored: bool) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must not succeed: {stderr}");
    assert_eq!(
        stderr.contains("carla-example"),
        stored,
        "refusal must name exactly the stored holders: {stderr}"
    );
    assert!(
        !stderr.contains("5550132") && !stderr.contains("555-0132"),
        "refusal must not echo the digits: {stderr}"
    );
    assert!(
        !stderr.contains("COMPOSIO_API_KEY"),
        "the gate must refuse before the Gmail client is built: {stderr}"
    );
}

/// `COMPOSIO_API_KEY` is unset here: a body that passes fails on the client.
fn assert_passed(out: &Output) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("COMPOSIO_API_KEY") && !stderr.contains("carla-example"),
        "body must pass the gate: {stderr}"
    );
}

/// The body of the incident's draft: a contact list attributing a stored
/// third party's mobile to the owner.
fn incident_body() -> String {
    format!("Team contacts:\n  Owner — owner@example.com, {BODY_RENDERING}\n")
}

#[test]
fn compose_refuses_a_third_partys_stored_number_from_the_store_index_alone() {
    let fx = Fixture::new().with_stored_phone();
    assert_refused(&fx.run(false, &COMPOSE, &incident_body(), &[]), true);
    assert_eq!(fx.action_count(), 0, "no approval card row may exist");
}

#[test]
fn compose_refuses_via_the_wiki_root_env_var_when_only_a_page_carries_the_number() {
    let fx = Fixture::new().with_wiki_page("carla@example.com", STORED_E164);
    assert_refused(&fx.run(true, &COMPOSE, &incident_body(), &[]), true);
    assert_eq!(fx.action_count(), 0);
}

/// The incident as reported: the number reached the draft from nowhere, so
/// no record claims it. Untraceable PII must be omitted, not guessed.
#[test]
fn compose_refuses_a_number_no_record_claims() {
    let fx = Fixture::new();
    assert_refused(&fx.run(false, &COMPOSE, &incident_body(), &[]), false);
    assert_eq!(fx.action_count(), 0);
}

#[test]
fn update_draft_and_send_now_run_the_same_gate() {
    let fx = Fixture::new().with_stored_phone();
    for op in [vec!["update-draft", "--draft-id", "r-1"], vec!["send-now"]] {
        assert_refused(&fx.run(false, &op, &incident_body(), &[]), true);
    }
}

#[test]
fn a_declared_number_or_a_recipients_own_number_passes_the_gate() {
    let fx = Fixture::new().with_stored_phone();
    let declared = ["--shared-contact", "carla-example"];
    assert_passed(&fx.run(false, &COMPOSE, &incident_body(), &declared));

    let fx = Fixture::new().with_wiki_page("client@example.org", STORED_E164);
    assert_passed(&fx.run(true, &COMPOSE, &incident_body(), &[]));
}

/// The recipient's number is indexed only in the store, which carries no
/// addresses: their wiki page (email only) is what ties the slug to them.
#[test]
fn a_recipients_store_indexed_number_passes_the_gate() {
    let fx = Fixture::new()
        .with_stored_phone()
        .with_wiki_page("client@example.org", "");
    assert_passed(&fx.run(true, &COMPOSE, &incident_body(), &[]));
}
