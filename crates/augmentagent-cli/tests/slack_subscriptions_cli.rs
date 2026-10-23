//! #1296 — managing Slack subscriptions through the built binary: subscribe
//! by channel name, person (wiki identity) or ID, change mode, unsubscribe,
//! and clear non-zero failures for ambiguous and unknown targets.
//!
//! Composio is a local mockito server (`AUGMENTAGENT_TEST_COMPOSIO_BASE`,
//! debug builds only), credentials go to the plaintext test store
//! (`AUGMENTAGENT_INSECURE_CREDENTIAL_DIR`, never the Keychain), and every
//! ID is synthetic.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::json;

const TEAM: &str = "T0000001";

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    composio: String,
}

impl Env {
    fn new(composio: String) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state dir ü");
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("wiki").join("people")).unwrap();
        for (slug, title, id) in [
            ("alice-example", "Alice Example", "U000000B"),
            ("alex-one", "Alex One", "U000000C"),
            ("alex-two", "Alex Two", "U000000D"),
        ] {
            std::fs::write(
                root.join("wiki").join("people").join(format!("{slug}.md")),
                format!("---\nkind: person\nidentities:\n  slack: {id}\n---\n# {title}\n"),
            )
            .unwrap();
        }
        Env {
            _tmp: tmp,
            root,
            composio,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_augmentagent"))
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("xdg state"))
            .env("AUGMENTAGENT_DB", self.root.join("agent.db"))
            .env(
                "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
                self.root.join("creds"),
            )
            .env("AUGMENTAGENT_TEST_COMPOSIO_BASE", &self.composio)
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env("NO_COLOR", "1")
            .env("RUST_LOG", "warn")
            .arg("--wiki-dir")
            .arg(self.root.join("wiki"))
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn subscriptions(&self) -> Vec<(String, String, String)> {
        let out = self.ok(&["slack", "subscriptions", "--json", "true"]);
        let rows: Vec<serde_json::Value> = serde_json::from_str(out.trim()).unwrap();
        rows.iter()
            .map(|r| {
                (
                    r["channel_id"].as_str().unwrap().to_string(),
                    r["display_name"].as_str().unwrap().to_string(),
                    r["mode"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

fn conversations() -> serde_json::Value {
    json!({
        "successful": true,
        "data": {"channels": [
            {"id": "C0000001", "name": "general", "is_channel": true},
            {"id": "C0000002", "name": "launch", "is_channel": true},
            {"id": "C0000004", "name": "design", "is_channel": true},
            {"id": "C0000005", "name": "design", "is_channel": true, "is_private": true},
            {"id": "C0000006", "name": "mpdm-alice--bob--owner-1", "is_mpim": true},
            {"id": "D0000007", "is_im": true, "user": "U000000B"}
        ]}
    })
}

#[test]
fn subscriptions_are_managed_by_name_person_or_id() {
    let mut server = mockito::Server::new();
    let _list = server
        .mock("POST", "/api/v3/tools/execute/SLACK_LIST_CONVERSATIONS")
        .with_status(200)
        .with_body(conversations().to_string())
        .expect_at_least(1)
        .create();
    let env = Env::new(server.url());
    let auth = env.root.join("auth.json");
    std::fs::write(
        &auth,
        json!({
            "entity_id": "entity-test", "connection_id": "conn-test",
            "team_id": TEAM, "team_name": "Example", "user_id": "U000000A",
            "composio_api_key": "composio-test-000"
        })
        .to_string(),
    )
    .unwrap();
    env.ok(&["slack", "login", "--auth-json", auth.to_str().unwrap()]);

    let out = env.ok(&["slack", "subscribe", "#launch", "--mode", "digest"]);
    assert!(
        out.contains("Subscribed to #launch (`C0000002`) in digest mode."),
        "{out}"
    );
    let out = env.ok(&["slack", "subscribe", "Alice", "--mode", "priority"]);
    assert!(out.contains("DM with Alice Example (`D0000007`)"), "{out}");
    let out = env.ok(&[
        "slack",
        "subscribe",
        "mpdm-alice--bob--owner-1",
        "--mode",
        "store_only",
    ]);
    assert!(out.contains("group DM with alice, bob, owner"), "{out}");

    // Ambiguous and unknown targets fail, say why, and change nothing.
    let err = env.fails(&["slack", "subscribe", "#design", "--mode", "digest"]);
    assert!(
        err.contains("Which one?") && err.contains("C0000004") && err.contains("C0000005"),
        "{err}"
    );
    let err = env.fails(&["slack", "subscribe", "Alex", "--mode", "digest"]);
    assert!(
        err.contains("Which one?") && err.contains("Alex One"),
        "{err}"
    );
    let err = env.fails(&["slack", "subscribe", "#no-such-channel", "--mode", "digest"]);
    assert!(
        err.contains("Nothing changed") && err.contains("no-such-channel"),
        "{err}"
    );
    let err = env.fails(&["slack", "set-mode", "#general", "--mode", "digest"]);
    assert!(err.contains("not a subscribed"), "{err}");
    assert_eq!(env.subscriptions().len(), 3);

    let out = env.ok(&["slack", "set-mode", "launch", "--mode", "priority"]);
    assert!(out.contains("is now priority (was digest)"), "{out}");
    let out = env.ok(&["slack", "unsubscribe", "Alice Example"]);
    assert!(
        out.contains("Unsubscribed from DM with Alice Example"),
        "{out}"
    );
    assert_eq!(
        env.subscriptions(),
        vec![
            ("C0000002".into(), "#launch".into(), "priority".into()),
            (
                "C0000006".into(),
                "group DM with alice, bob, owner".into(),
                "store_only".into()
            ),
        ]
    );
    // By ID with an explicit name, and the old unsubscribe-by-row-id form.
    env.ok(&[
        "slack",
        "subscribe",
        "C0000009",
        "--mode",
        "digest",
        "--name",
        "#ops",
    ]);
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(env.ok(&["slack", "subscriptions", "--json", "true"]).trim()).unwrap();
    let ops = rows.iter().find(|r| r["channel_id"] == "C0000009").unwrap();
    env.ok(&["slack", "unsubscribe", ops["id"].as_str().unwrap()]);
    assert_eq!(env.subscriptions().len(), 2);
}
