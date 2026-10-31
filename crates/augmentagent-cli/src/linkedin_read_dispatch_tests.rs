//! Parse real CLI arguments and invoke the same dispatcher used by main.
use super::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use wiremock::{matchers::method, Mock, MockServer};
#[path = "../../augmentagent-channel-linkedin/tests/support/mod.rs"]
mod support;

async fn run(
    reader: &augmentagent_channel_linkedin::archive::ArchiveReader,
    args: Vec<String>,
) -> Value {
    let cli = Cli::try_parse_from(args).unwrap();
    let Cmd::Linkedin { op } = cli.cmd else {
        panic!("wrong CLI route");
    };
    serde_json::from_str(&dispatch_linkedin_read(&op, reader).await.unwrap()).unwrap()
}

#[tokio::test]
async fn parsed_cli_reads_all_intro_threads_and_full_owner_replies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|r: &wiremock::Request| {
            let q = r.url.query_pairs().collect::<BTreeMap<_, _>>();
            if q.get("queryId").is_some_and(|q| q.contains("fb3eab")) {
                let variables = q["variables"].replace("%3A", ":");
                let i = variables
                    .split("thread:")
                    .nth(1)
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                wiremock::ResponseTemplate::new(200).set_body_json(
                    json!({"data":{"messengerConversationsById":support::conversation(i)}}),
                )
            } else {
                support::respond(r)
            }
        })
        .mount(&server)
        .await;
    let reader = augmentagent_channel_linkedin::archive::ArchiveReader::for_test(
        support::auth(),
        server.uri(),
    )
    .unwrap();
    let base = vec![
        "augmentagent",
        "linkedin",
        "recent-dms",
        "--limit",
        "40",
        "--category",
        "PRIMARY_INBOX",
        "--json",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    let first = run(&reader, base.clone()).await;
    assert_eq!(first["schema_version"], 2);
    assert_eq!(first["conversations"].as_array().unwrap().len(), 40);
    assert_eq!(first["pages"], 2);
    assert_eq!(first["complete"], false);
    let manifest: Value = serde_json::from_str(include_str!(
        "../../augmentagent-channel-linkedin/tests/fixtures/archive_manifest.json"
    ))
    .unwrap();
    assert_eq!(
        first["conversations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["urn"].clone())
            .collect::<Vec<_>>(),
        manifest["recent_40_ids"].as_array().unwrap().clone()
    );
    let mut continuation = base;
    continuation.extend([
        "--cursor".into(),
        first["next_cursor"].as_str().unwrap().into(),
    ]);
    let second = run(&reader, continuation).await;
    assert_eq!(second["complete"], true);
    let all = first["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["conversations"].as_array().unwrap());
    let groups: BTreeSet<String> = all
        .filter(|c| {
            c["participants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["urn"] == "introducer-1")
        })
        .map(|c| c["urn"].as_str().unwrap().into())
        .collect();
    assert_eq!(
        groups,
        ["thread:2", "thread:22", "thread:42"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    for thread in groups {
        let base = vec![
            "augmentagent".into(),
            "linkedin".into(),
            "thread".into(),
            thread.clone(),
            "--max-pages".into(),
            "1".into(),
            "--json".into(),
        ];
        let mut cursor = None;
        let mut messages = BTreeMap::new();
        let mut exhausted = false;
        for _ in 0..4 {
            let mut args = base.clone();
            if let Some(cursor) = cursor {
                args.extend(["--cursor".into(), cursor]);
            }
            let page = run(&reader, args).await;
            assert_eq!(page["account_id"], format!("linkedin:{}", support::OWNER));
            for m in page["messages"].as_array().unwrap() {
                assert!(messages
                    .insert(m["urn"].as_str().unwrap().to_string(), m.clone())
                    .is_none());
            }
            if page["complete"] == true {
                exhausted = true;
                break;
            }
            cursor = Some(page["next_cursor"].as_str().unwrap().to_string());
        }
        assert!(exhausted);
        assert_eq!(messages.len(), 9);
        let i = thread
            .strip_prefix("thread:")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        for j in 0..9 {
            let m = &messages[&format!("message:{i}:{j}")];
            assert_eq!(m["body"], support::body(i, j));
            assert_eq!(m["from_me"], j % 3 == 2);
            assert_eq!(m["delivered_at_ms"], 1000 + j / 2);
        }
    }
    // 3 mailbox pages plus detail+history for each of 9 thread pages.
    assert_eq!(server.received_requests().await.unwrap().len(), 21);
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method.as_str() == "GET"));
}
