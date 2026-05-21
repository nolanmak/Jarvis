//! Snapshot test that locks the `augmentagent status --json` document shape
//! at `schema_version: "1"`.
//!
//! ## Why a snapshot test
//!
//! The `/setup` skill (issue #5) parses this JSON. The schema reference at
//! `skills/augmentagent-setup/reference/status-schema.md` documents it. The
//! producer is `status::collect()` in this crate. Without a pinned snapshot,
//! a careless edit to `status.rs` can rename a key or change a type and the
//! skill silently breaks in production.
//!
//! This test fails on ANY change to the JSON shape — adding a field,
//! removing one, changing a type. Reviewers then look at the `.snap` diff
//! and either:
//!   1. accept the change (bump `SCHEMA_VERSION` first, then `cargo insta
//!      accept`), or
//!   2. revert the producer.
//!
//! ## Subprocess, not library
//!
//! `augmentagent-cli` is a binary-only crate (no `lib.rs`), so integration
//! tests can't import `status::collect` directly. We instead invoke the
//! built `augmentagent` binary via `CARGO_BIN_EXE_augmentagent` and parse
//! the JSON it writes to stdout. Cargo builds the bin once per test run
//! and exposes its path through that env var.
//!
//! Library-mode invocation (calling `status::collect(&Store)`) was
//! considered but rejected: making the cli crate a library would require
//! touching `main.rs` and adding a `lib.rs`, which is outside this test's
//! allowlist. Subprocess invocation is a cleaner contract: it also
//! exercises clap-flag handling and the actual JSON printer end-to-end.
//!
//! ## Synthetic store + isolated env
//!
//! `status::collect` reads the sqlite `config` table via `AUGMENTAGENT_DB`.
//! We point that env var at an empty database in a `TempDir`, so
//! `core_keys` and `channels[*].configured` all resolve to `false`. We
//! also wipe the env vars the aggregator probes (`AUGMENTAGENT_API_KEY`,
//! `DASHBOARD_PORT`, every per-channel credential key) so a developer
//! running the test on a real configured box still gets the "fresh
//! install" shape.
//!
//! ## Redactions
//!
//! Some fields depend on host state we cannot control from a unit test:
//!   * `daemon.active`, `daemon.since_unix`, `dashboard.active`,
//!     `dashboard.reachable`, `updater.timer_active`, `updater.last_run_unix`
//!     — driven by `systemctl --user show` on the box the test runs on.
//!   * `summary` — derived from the above; flips between `daemon_down`
//!     (CI, no user systemd) and `ok` / `degraded` (dev box).
//!
//! We use `insta`'s redaction syntax to mask those values with the literal
//! string `"[volatile]"`. The KEYS stay in the snapshot, so a rename still
//! breaks the test; only the volatile VALUES are masked.

use std::process::Command;

use insta::assert_json_snapshot;
use rusqlite::Connection;
use tempfile::TempDir;

/// Minimum legacy schema required by `augmentagent_store::Store::migrate()`.
/// The store crate's own tests pre-seed the same three tables (see
/// `store.rs::fresh_store`). The migrate path only `ALTER`s columns onto
/// them; if they're missing, `Store::open` aborts with "no such table:
/// actions" and the binary exits before printing any JSON. We don't insert
/// rows — empty tables are enough for the aggregator to produce its
/// "fresh install" shape.
const LEGACY_SCHEMA_PREAMBLE: &str = r#"
    CREATE TABLE actions (
        id TEXT PRIMARY KEY,
        messageId TEXT NOT NULL,
        threadId TEXT,
        fromEmail TEXT NOT NULL,
        subject TEXT NOT NULL,
        originalBody TEXT,
        draftBody TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        errorMessage TEXT,
        createdAt INTEGER NOT NULL,
        updatedAt INTEGER NOT NULL
    );
    CREATE TABLE emails (
        messageId TEXT PRIMARY KEY,
        threadId TEXT,
        fromEmail TEXT NOT NULL,
        subject TEXT NOT NULL,
        body TEXT,
        receivedAt TEXT,
        accountEntityId TEXT,
        firstSeenAt INTEGER NOT NULL,
        triageResult TEXT,
        agentProcessedAt INTEGER,
        platform TEXT NOT NULL DEFAULT 'gmail',
        kind TEXT NOT NULL DEFAULT 'dm'
    );
    CREATE TABLE gmail_accounts (
        id TEXT PRIMARY KEY,
        connectionId TEXT NOT NULL,
        email TEXT,
        label TEXT,
        entityId TEXT NOT NULL,
        active INTEGER DEFAULT 1,
        createdAt INTEGER NOT NULL
    );
"#;

/// Env vars `status::collect()` may consult. Cleared in the child process
/// so the "configured" booleans are deterministic regardless of the
/// developer's shell environment.
const ENV_VARS_TO_CLEAR: &[&str] = &[
    "AUGMENTAGENT_API_KEY",
    "DASHBOARD_PORT",
    // Core keys.
    "COMPOSIO_API_KEY",
    "GROQ_API_KEY",
    "CEREBRAS_API_KEY",
    "DISCORD_BOT_TOKEN",
    // Per-channel env keys (see `collect_channels` in status.rs).
    "SLACK_BOT_TOKEN",
    "TWITTER_SESSION_B64",
    "LINKEDIN_LI_AT",
    "INSTAGRAM_SESSION_B64",
    "REDDIT_REFRESH_TOKEN",
    "GITHUB_TOKEN",
    "MEETUP_ACCESS_TOKEN",
    "TELEGRAM_BOT_TOKEN",
    "WHATSAPP_SESSION_B64",
    "VOICE_DROP_DIR",
    "CARDDAV_URL",
];

#[test]
fn status_json_matches_locked_schema_v1() {
    // ---- synthetic db path -------------------------------------------
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("status_schema.db");

    // Pre-seed the legacy tables so `Store::open`'s `migrate()` doesn't
    // abort the binary before it can emit JSON.
    {
        let conn = Connection::open(&db_path).expect("open seed db");
        conn.execute_batch(LEGACY_SCHEMA_PREAMBLE)
            .expect("seed legacy schema");
    }

    // ---- invoke the binary -------------------------------------------
    //
    // `CARGO_BIN_EXE_augmentagent` is set automatically by cargo when an
    // integration test depends on a `[[bin]]` named `augmentagent` in the
    // same crate. Cargo builds the bin once and reuses it across tests.
    let bin = env!("CARGO_BIN_EXE_augmentagent");

    let mut cmd = Command::new(bin);
    cmd.arg("status").arg("--json").arg("true");
    // Run from the tempdir so `dotenvy::dotenv()` in main can't pick up a
    // developer `.env` higher up the tree and pollute the result.
    cmd.current_dir(tmp.path());
    // Force a clean, empty sqlite store. The aggregator reads
    // `AUGMENTAGENT_DB` (status.rs:434), and main.rs also reads it (line
    // 1362) for the Store path. Pinning it to the tempdir means we don't
    // touch the developer's data.db.
    cmd.env("AUGMENTAGENT_DB", &db_path);
    // Wipe every other env var the aggregator inspects so the shape is
    // identical on a fresh CI box and on a fully-configured dev box.
    for k in ENV_VARS_TO_CLEAR {
        cmd.env_remove(k);
    }

    let out = cmd.output().expect("spawn augmentagent status");
    // `status` exits non-zero on a "needs setup" / "daemon down" box
    // (which is what CI will look like). We only care about stdout JSON
    // here; exit-code semantics are covered by the unit tests in
    // `status.rs` itself.
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "status did not emit valid JSON: {e}\nexit={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status.code()
        )
    });

    // ---- snapshot with redactions ------------------------------------
    //
    // Volatile values get replaced with `"[volatile]"`. Keys + everything
    // else stays in the snapshot, which is what locks the contract.
    assert_json_snapshot!("status_v1", json, {
        ".daemon.active" => "[volatile]",
        ".daemon.since_unix" => "[volatile]",
        ".dashboard.active" => "[volatile]",
        ".dashboard.reachable" => "[volatile]",
        ".updater.timer_active" => "[volatile]",
        ".updater.last_run_unix" => "[volatile]",
        ".summary" => "[volatile]",
    });
}
