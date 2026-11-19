use anyhow::Result;
use augmentagent_store::{ActionStatus, Store};

#[allow(clippy::too_many_arguments)]
fn social_card_row(
    _platform: &str,
    _kind: &str,
    _message_id: &str,
    _thread_id: &str,
    _account: Option<&str>,
    _counterparty: &str,
    _subject: &str,
    _body: &str,
) -> augmentagent_store::Email {
    unreachable!("the observation only calls pending-compose selection and retirement")
}

#[path = "../socialapi_compose_identity.rs"]
mod socialapi_compose_identity;

const CONVERSATION: &str = "contract-conversation";
const LEGACY_MESSAGE: &str = "compose:socialapi:dm:contract-conversation";
const FIRST_MESSAGE: &str = "compose:socialapi:dm:contract-conversation:first";
const SECOND_MESSAGE: &str = "compose:socialapi:dm:contract-conversation:second";
const TRIAGE_MESSAGE: &str = "contract-inbound";
const OTHER_MESSAGE: &str = "compose:socialapi:dm:other-conversation:card";
const SCHEDULED_MESSAGE: &str = "compose:socialapi:dm:contract-conversation:scheduled";
const LEGACY: &str = "c0000000-0000-0000-0000-000000000001";
const FIRST: &str = "b0000000-0000-0000-0000-000000000002";
const SECOND: &str = "a0000000-0000-0000-0000-000000000003";

fn seed_email(store: &Store, message_id: &str, thread_id: &str) -> Result<()> {
    store.with_conn(|conn| {
        conn.execute(
            "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, firstSeenAt, platform, kind) VALUES (?1, ?2, 'contract-peer', 'contract subject', 'contract body', 0, 'socialapi', 'dm')",
            augmentagent_store::rusqlite::params![message_id, thread_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn add_action(store: &Store, message_id: &str, thread_id: &str) -> Result<String> {
    Ok(store.log_action(
        message_id,
        Some(thread_id),
        "contract-peer",
        "contract subject",
        Some("contract body"),
        Some("contract draft"),
        ActionStatus::Pending,
    )?)
}

fn label(id: &str) -> &'static str {
    match id {
        LEGACY => "legacy",
        FIRST => "first",
        SECOND => "second",
        _ => "unknown",
    }
}

fn main() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("contract.db"))?;

    for (message, thread) in [
        (TRIAGE_MESSAGE, CONVERSATION),
        (LEGACY_MESSAGE, CONVERSATION),
        (FIRST_MESSAGE, CONVERSATION),
        (SECOND_MESSAGE, CONVERSATION),
        (OTHER_MESSAGE, "other-conversation"),
        (SCHEDULED_MESSAGE, CONVERSATION),
    ] {
        seed_email(&store, message, thread)?;
        add_action(&store, message, thread)?;
    }

    store.with_conn(|conn| {
        for (id, message) in [
            (LEGACY, LEGACY_MESSAGE),
            (FIRST, FIRST_MESSAGE),
            (SECOND, SECOND_MESSAGE),
        ] {
            conn.execute(
                "UPDATE actions SET id = ?1, createdAt = 1000 WHERE messageId = ?2",
                augmentagent_store::rusqlite::params![id, message],
            )?;
        }
        conn.execute(
            "UPDATE actions SET scheduledAtMs = 1 WHERE messageId = ?1",
            [SCHEDULED_MESSAGE],
        )?;
        Ok(())
    })?;

    let prior = socialapi_compose_identity::pending_compose_cards(&store, CONVERSATION)?;
    let order: Vec<&str> = prior.iter().map(|id| label(id)).collect();

    store.update_action_status(FIRST, ActionStatus::Sent, None, None)?;
    let retired = socialapi_compose_identity::supersede_prior_compose_cards(&store, &prior);
    let retired: Vec<&str> = retired.iter().map(|id| label(id)).collect();

    let statuses: Result<Vec<String>> = [LEGACY, FIRST, SECOND]
        .iter()
        .chain([TRIAGE_MESSAGE, OTHER_MESSAGE, SCHEDULED_MESSAGE].iter())
        .map(|id| {
            let action_id = match *id {
                TRIAGE_MESSAGE | OTHER_MESSAGE | SCHEDULED_MESSAGE => store.with_conn(|conn| {
                    conn.query_row(
                        "SELECT id FROM actions WHERE messageId = ?1",
                        [*id],
                        |row| row.get::<_, String>(0),
                    )
                })?,
                _ => (*id).to_owned(),
            };
            Ok(store.get_action_with_email(&action_id)?.unwrap().action.status)
        })
        .collect();

    let remaining: Vec<&str> = socialapi_compose_identity::pending_compose_cards(&store, CONVERSATION)?
        .iter()
        .map(|id| label(id))
        .collect();

    println!("{}", serde_json::json!([order, retired, statuses?, remaining]));
    Ok(())
}
