use augmentagent_store::{approval_cards::ActionRefMatch, ActionStatus, Store};
use serde_json::json;

fn email(store: &Store, id: &str, platform: &str, account: &str, thread: &str) {
    store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO emails(messageId,threadId,fromEmail,subject,body,firstSeenAt,platform,accountEntityId,kind) VALUES (?1,?2,'synthetic@example.invalid','synthetic','body',1,?3,?4,'dm')",
                augmentagent_store::rusqlite::params![id, thread, platform, account],
            )?;
            Ok(())
        })
        .unwrap();
}

fn action(store: &Store, message_id: &str) -> String {
    store
        .log_action(
            message_id,
            None,
            "synthetic@example.invalid",
            "synthetic",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap()
}

fn main() {
    let path = std::env::temp_dir().join(format!("autoship-draft-context-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    let store = Store::open(path.join("db")).unwrap();

    email(&store, "social-source", "socialapi", "account-a", "conversation-42");
    email(&store, "social-action", "socialapi", "account-a", "instagram:conversation-42");
    let social_receipt = store
        .save_draft_context_receipt("social-source", &json!({"version": 1}))
        .unwrap();
    let social_action = action(&store, "social-action");
    let social_attached = store
        .attach_draft_context_receipt(&social_receipt, &social_action)
        .unwrap();

    email(&store, "wrong-platform", "imessage", "account-a", "conversation-42");
    email(&store, "wrong-account", "socialapi", "account-b", "instagram:conversation-42");
    email(&store, "wrong-conversation", "socialapi", "account-a", "instagram:conversation-99");
    let boundary_receipt = store
        .save_draft_context_receipt("social-source", &json!({"version": 1}))
        .unwrap();
    let wrong_platform = !store
        .attach_draft_context_receipt(&boundary_receipt, &action(&store, "wrong-platform"))
        .unwrap();
    let wrong_account = !store
        .attach_draft_context_receipt(&boundary_receipt, &action(&store, "wrong-account"))
        .unwrap();
    let wrong_conversation = !store
        .attach_draft_context_receipt(&boundary_receipt, &action(&store, "wrong-conversation"))
        .unwrap();

    let assigned_receipt = store
        .save_draft_context_receipt("social-source", &json!({"version": 1}))
        .unwrap();
    let first = action(&store, "social-action");
    let second = action(&store, "social-action");
    let single_assignment = store
        .attach_draft_context_receipt(&assigned_receipt, &first)
        .unwrap()
        && store
            .attach_draft_context_receipt(&assigned_receipt, &first)
            .unwrap()
        && !store
            .attach_draft_context_receipt(&assigned_receipt, &second)
            .unwrap();

    let direct_receipt = store
        .save_draft_context_receipt("social-source", &json!({"version": 1}))
        .unwrap();
    let direct_message = action(&store, "social-source");
    let direct_attachment = store
        .attach_draft_context_receipt(&direct_receipt, &direct_message)
        .unwrap();

    for fixed in [
        "abcdef01-0000-4000-8000-000000000001",
        "abcdef01-0000-4000-8000-000000000002",
    ] {
        let generated = action(&store, "social-action");
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE actions SET id=?1 WHERE id=?2",
                    augmentagent_store::rusqlite::params![fixed, generated],
                )?;
                Ok(())
            })
            .unwrap();
    }
    let ambiguous_receipt = store
        .save_draft_context_receipt("social-source", &json!({"version": 1}))
        .unwrap();
    let ambiguous = matches!(
        store.resolve_action_ref("abcdef01").unwrap(),
        ActionRefMatch::Ambiguous(_)
    ) && !store
        .attach_draft_context_receipt(&ambiguous_receipt, "abcdef01")
        .unwrap();
    let boundaries = wrong_platform
        && wrong_account
        && wrong_conversation
        && single_assignment
        && direct_attachment
        && ambiguous;

    email(&store, "imessage-source", "imessage", "imessage-account", "peer@example.invalid");
    email(&store, "imessage-compose", "imessage", "imessage-account", "peer@example.invalid");
    let imessage_receipt = store
        .save_draft_context_receipt("imessage-source", &json!({"version": 1}))
        .unwrap();
    let imessage_action = action(&store, "imessage-compose");
    let short = imessage_action[..8].to_ascii_uppercase();
    let imessage = match store.resolve_action_ref(&short).unwrap() {
        ActionRefMatch::One(id) => store
            .attach_draft_context_receipt(&imessage_receipt, &id)
            .unwrap(),
        _ => false,
    };

    drop(store);
    let reopened = Store::open(path.join("db")).unwrap();
    let durable = reopened.draft_context_receipts(&social_action).unwrap().len() == 1;
    println!("{}", json!([social_attached, boundaries, imessage, durable]));
    drop(reopened);
    let _ = std::fs::remove_dir_all(path);
}
