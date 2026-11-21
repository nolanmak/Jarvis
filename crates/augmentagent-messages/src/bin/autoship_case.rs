use augmentagent_messages::{
    index::drain,
    people,
    stats::{stats, GroupBy, StatsRequest, StatsResponse},
};
use augmentagent_store::{Email, Store};
use serde_json::json;
use std::time::Duration;

fn run(store: &Store, request: StatsRequest) -> StatsResponse {
    store.with_conn(|connection| Ok(stats(connection, &request).unwrap())).unwrap()
}

fn main() {
    let root = std::env::temp_dir().join(format!("autoship-case-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("people")).unwrap();
    std::fs::write(
        root.join("people/alex-kim.md"),
        "---\nkind: person\nidentities:\n  phone: [\"+14155550123\"]\n---\n",
    )
    .unwrap();
    let store = Store::open(root.join("messages.db")).unwrap();
    let put = |id: &str, platform: &str, from: &str, thread: &str, subject: &str, kind: &str, date: &str| {
        store
            .upsert_email(&Email {
                message_id: id.into(),
                thread_id: Some(thread.into()),
                from: from.into(),
                to: String::new(),
                cc: String::new(),
                attachments: vec![],
                subject: subject.into(),
                body: "fixture".into(),
                date: date.into(),
                account_entity_id: Some("discord:900".into()),
                platform: platform.into(),
                kind: kind.into(),
            })
            .unwrap();
    };
    put(
        "whatsapp-resolved",
        "whatsapp",
        "+14155550123",
        "whatsapp:group:alpha",
        "WhatsApp: Alpha",
        "group",
        "2026-09-29T10:00:00Z",
    );
    put(
        "unclaimed",
        "discord",
        "Sam <discord:600>",
        "discord:dm:600",
        "Discord DM: Sam",
        "dm",
        "2026-09-30T10:00:00Z",
    );
    drain(&store, 100, Duration::ZERO).unwrap();
    people::resolve_people(&store, &root).unwrap();
    store
        .set_archive_newest_entry("whatsapp", Some("2026-09-29T21:10:00-04:00"))
        .unwrap();

    let person = run(&store, StatsRequest::default());
    let flag = |response: &StatsResponse, key: &str| {
        response.rows.iter().find(|row| row.key == key).map(|row| row.unresolved)
    };
    let non_person = [
        GroupBy::Conversation,
        GroupBy::Platform,
        GroupBy::Kind,
        GroupBy::Day,
        GroupBy::Week,
        GroupBy::Month,
    ]
    .into_iter()
    .map(|group_by| {
        let response = run(
            &store,
            StatsRequest {
                group_by,
                ..Default::default()
            },
        );
        response.rows.iter().all(|row| !row.unresolved)
    })
    .collect::<Vec<_>>();
    let conversation = run(
        &store,
        StatsRequest {
            group_by: GroupBy::Conversation,
            ..Default::default()
        },
    );
    let stale = run(
        &store,
        StatsRequest {
            group_by: GroupBy::Conversation,
            platforms: vec!["whatsapp".into()],
            since_ms: Some(1_790_812_800_000),
            ..Default::default()
        },
    );
    let newest = stale
        .archive_newest_entry
        .get("whatsapp")
        .cloned()
        .flatten();
    println!(
        "{}",
        json!([
            [flag(&person, "alex-kim"), flag(&person, "discord:600")],
            non_person,
            flag(&conversation, "whatsapp:group:alpha"),
            [newest, Some(stale.stale_platforms == vec!["whatsapp".to_string()])]
        ])
    );
    let _ = std::fs::remove_dir_all(root);
}
