use augmentagent_channel_core::draft_context::{load_latest_seed, load_seed, ContextSeed};
use augmentagent_store::{Email, Store};
use serde_json::{json, Value};

fn email(
    id: &str,
    thread: Option<&str>,
    account: &str,
    platform: &str,
    from: &str,
    minute: u8,
    kind: &str,
) -> Email {
    Email {
        message_id: id.into(),
        thread_id: thread.map(str::to_owned),
        from: from.into(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        subject: "synthetic".into(),
        body: id.into(),
        date: format!("2026-01-01T00:{minute:02}:00Z"),
        account_entity_id: Some(account.into()),
        platform: platform.into(),
        kind: kind.into(),
    }
}

fn put(store: &Store, row: &Email) {
    store.upsert_email_backfill(row, 1).unwrap();
}

fn targets(seed: &ContextSeed) -> Vec<String> {
    serde_json::to_value(seed)
        .unwrap()
        .get("reply_targets")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    row.get("message_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn store(name: &str) -> (Store, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("autoship-2176-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    (Store::open(path.join("data.db")).unwrap(), path)
}

fn main() {
    let (normal, normal_path) = store("normal");
    put(
        &normal,
        &email("old-in", Some("t"), "a", "imessage", "peer", 1, "dm"),
    );
    put(
        &normal,
        &email("owner", Some("t"), "a", "imessage", "me", 2, "dm"),
    );
    put(
        &normal,
        &email("new-a", Some("t"), "a", "imessage", "peer", 3, "dm"),
    );
    let newest = email("new-b", Some("t"), "a", "imessage", "peer", 4, "dm");
    put(&normal, &newest);
    put(
        &normal,
        &email("draft", Some("t"), "a", "imessage", "me", 5, "compose"),
    );
    let normal_targets = targets(&load_seed(&normal, &newest, None).unwrap());
    let _ = std::fs::remove_dir_all(normal_path);

    let (latest, latest_path) = store("latest");
    put(
        &latest,
        &email("owner", Some("t"), "a", "imessage", "me", 1, "dm"),
    );
    let original = email("original", Some("t"), "a", "imessage", "peer", 2, "dm");
    put(&latest, &original);
    put(
        &latest,
        &email("latest-a", Some("t"), "a", "imessage", "peer", 3, "dm"),
    );
    put(
        &latest,
        &email("latest-b", Some("t"), "a", "imessage", "peer", 4, "dm"),
    );
    let latest_targets = targets(&load_latest_seed(&latest, &original, None).unwrap());
    let _ = std::fs::remove_dir_all(latest_path);

    let (scoped, scoped_path) = store("scope");
    let scoped_trigger = email(
        "scope-a",
        Some("thread-a"),
        "account-a",
        "imessage",
        "peer",
        2,
        "dm",
    );
    put(&scoped, &scoped_trigger);
    put(
        &scoped,
        &email(
            "other-account",
            Some("thread-a"),
            "account-b",
            "imessage",
            "peer",
            3,
            "dm",
        ),
    );
    put(
        &scoped,
        &email(
            "other-platform",
            Some("thread-a"),
            "account-a",
            "slack",
            "peer",
            4,
            "dm",
        ),
    );
    put(
        &scoped,
        &email(
            "other-thread",
            Some("thread-b"),
            "account-a",
            "imessage",
            "peer",
            5,
            "dm",
        ),
    );
    put(
        &scoped,
        &email(
            "scope-compose",
            Some("thread-a"),
            "account-a",
            "imessage",
            "me",
            6,
            "compose",
        ),
    );
    let scoped_targets = targets(&load_seed(&scoped, &scoped_trigger, None).unwrap());
    let _ = std::fs::remove_dir_all(scoped_path);

    let (missing, missing_path) = store("missing");
    let no_thread = email("no-thread", None, "a", "imessage", "peer", 1, "dm");
    let no_thread_seed = load_seed(&missing, &no_thread, None).unwrap();
    let missing_gap = no_thread_seed
        .gaps
        .iter()
        .any(|gap| gap == "Conversation identifier unavailable");
    let _ = std::fs::remove_dir_all(missing_path);

    println!(
        "{}",
        json!([normal_targets, latest_targets, scoped_targets, missing_gap])
    );
}
