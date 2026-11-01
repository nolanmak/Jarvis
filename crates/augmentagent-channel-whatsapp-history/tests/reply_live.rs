//! #1764 — explicit opt-in check of the group bar with the real reasoner chain.
//! Synthetic data only; the broker records cards, nothing reaches Discord or
//! WhatsApp. Run: cargo test -p augmentagent-channel-whatsapp-history
//!   --test reply_live -- --ignored --nocapture
use std::sync::{Arc, Mutex};

use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::{build_reasoner, Reasoner};
use augmentagent_channel_imessage::{Conversation, MessageEntry};
use augmentagent_channel_whatsapp_history::{email, reply::WhatsappReplier, Delta};
use augmentagent_store::{Email, Store};

#[derive(Default)]
struct Recorder(Mutex<Vec<(Email, String)>>);

#[async_trait::async_trait]
impl ApprovalBroker for Recorder {
    async fn post_approval(&self, _id: &str, e: &Email, d: &str) -> Result<(), ApprovalError> {
        self.0.lock().unwrap().push((e.clone(), d.to_string()));
        Ok(())
    }
    async fn post_flag_notice(&self, _e: &Email, _r: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
}

/// Prints each model reply so a failing run shows the reasoning.
struct Traced(Arc<dyn Reasoner>);

#[async_trait::async_trait]
impl Reasoner for Traced {
    async fn call(&self, o: &augmentagent_channel_core::ReasonerOpts, m: &str) -> anyhow::Result<String> {
        let r = self.0.call(o, m).await;
        eprintln!("LIVE_REPLY: {}", r.as_ref().map(|s| s.chars().take(300).collect::<String>()).unwrap_or_else(|e| format!("ERR {e:#}")));
        r
    }
}

fn group(id: &str) -> Conversation {
    Conversation {
        identifier: id.into(),
        dir: id.into(),
        title: "Synthetic Ski Trip".into(),
        participants: vec!["+15555550111".into(), "+15555550122".into()], // pii-ok synthetic
        participant_names: [("+15555550111".to_string(), "Riley".to_string()), ("+15555550122".to_string(), "Morgan".to_string())] // pii-ok synthetic
            .into_iter()
            .collect(),
        service: "WhatsApp".into(),
        ..Default::default()
    }
}

fn delta(id: &str, entries: &[(&str, &str)]) -> Delta {
    let now = chrono::Local::now();
    Delta {
        conversation: group(id),
        entries: entries
            .iter()
            .enumerate()
            .map(|(i, (sender, body))| {
                let ts = now - chrono::Duration::minutes((entries.len() - i) as i64);
                (i + 1, MessageEntry { timestamp: ts.to_rfc3339(), sender: (*sender).into(), body: (*body).into(), attachments: vec![] })
            })
            .collect(),
        first_run: false,
    }
}

#[tokio::test]
#[ignore = "requires authenticated model providers; synthetic fixtures only"]
async fn real_reasoner_drafts_for_a_question_to_the_owner_and_skips_group_chatter() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("d.db")).unwrap());
    store.set_whatsapp_contact_defaults(true, false).unwrap();
    let broker = Arc::new(Recorder::default());
    let replier = WhatsappReplier::new(Arc::clone(&store), Arc::new(Traced(build_reasoner())) as Arc<dyn Reasoner>, broker.clone(), None);
    // The model only treats a name as the owner's when it is: take it from the
    // deployment's owner aliases (local .env), never from code.
    let owner = std::env::var("AUGMENTAGENT_LIVE_OWNER_NAME")
        .ok()
        .or_else(|| std::env::var("AUGMENTAGENT_OWNER_ALIASES").ok()?.split(',').next().map(|a| a.trim().to_string()))
        .filter(|n| !n.is_empty())
        .expect("set AUGMENTAGENT_LIVE_OWNER_NAME or AUGMENTAGENT_OWNER_ALIASES to the owner's first name");
    let asked = delta("120363000000000001@g.us", &[
        ("+15555550111", "lodge is booked for friday"), // pii-ok synthetic
        ("+15555550122", &format!("{owner}, can you drive the second car friday morning? need to know by tonight")), // pii-ok synthetic
    ]);
    let chatter = delta("120363000000000002@g.us", &[
        ("+15555550111", "lol that video"), // pii-ok synthetic
        ("+15555550122", "😂😂 classic"), // pii-ok synthetic
    ]);
    for d in [&asked, &chatter] {
        for (i, e) in &d.entries {
            store.upsert_email(&email(&d.conversation, *i, e)).unwrap();
        }
    }
    let stats = replier.handle_deltas(&[asked, chatter]).await;
    eprintln!("LIVE_STATS: {stats:?}");
    let cards = broker.0.lock().unwrap().clone();
    for (card, draft) in &cards {
        eprintln!("LIVE_CARD: subject={} to={} draft={draft}", card.subject, card.to);
    }
    assert_eq!(stats.errors, 0, "{stats:?}");
    assert_eq!(cards.len(), 1, "exactly the question to the owner gets a card: {stats:?}");
    assert!(cards[0].0.subject.contains("Synthetic Ski Trip") && cards[0].0.to.contains("Riley"));
    assert_eq!(stats.flagged, 0, "chatter is skipped, not flagged");
}
