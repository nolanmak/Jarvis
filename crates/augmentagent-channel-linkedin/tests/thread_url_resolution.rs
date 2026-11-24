//! Offline resolution of a messaging-thread URL. No HTTP client is built here,
//! so a passing test is also proof the lookup is archive-only.
use augmentagent_channel_linkedin::{
    archive::{ArchiveError, Conversation, Page, Participant},
    archive_store::{checkpoint, commit_conversations},
    session_guard,
    thread_url_resolution::{
        parse_thread_url, resolve_thread_url, resolve_thread_url_report, session_advice,
        ThreadUrlError, ThreadUrlResolution, RECONNECT_NOTE,
    },
};
use augmentagent_store::Store;

const OWNER: &str = "urn:li:fsd_profile:OWNER";
const OTHER: &str = "urn:li:fsd_profile:OTHER";
/// Shaped like the id in the reported link: opaque, prefixed, base64 padding.
const THREAD: &str = "2-placeholder-thread-id==";

fn participant(urn: &str, name: &str) -> Participant {
    Participant {
        urn: urn.into(),
        name: name.into(),
    }
}

fn url() -> String {
    format!("https://www.linkedin.com/messaging/thread/{THREAD}/")
}

/// Sync records the mailbox member urn as the account and as a participant.
fn sync(store: &Store, account: &str, group: bool, peers: Vec<Participant>) {
    let cp = checkpoint(store, account, "PRIMARY_INBOX").unwrap();
    let items = vec![Conversation {
        urn: THREAD.into(),
        title: "Thread".into(),
        group,
        last_activity_ms: 100,
        participants: std::iter::once(participant(account, "Owner"))
            .chain(peers)
            .collect(),
    }];
    assert!(
        commit_conversations(
            store,
            account,
            "PRIMARY_INBOX",
            &cp,
            &Page { items, next: None },
            10_000,
        )
        .unwrap()
    );
}

fn resolve(store: &Store, account: &str, url: &str) -> ThreadUrlResolution {
    resolve_thread_url(store, account, url).unwrap()
}

/// The reported case: the live session is rejected, yet the pasted thread URL
/// still names the person, because resolution reads the archive instead.
#[test]
fn a_stale_session_still_resolves_an_archived_thread_url_to_its_participant() {
    let store = Store::open(":memory:").unwrap();
    let redirect = ArchiveError::Redirect.to_string();
    session_guard::record_failure(&store, OWNER, &redirect, 1_700_000_000_000).unwrap();
    sync(
        &store,
        OWNER,
        false,
        vec![participant("person:1", "Dana Example")],
    );

    let resolved = resolve(
        &store,
        OWNER,
        &format!("https://www.linkedin.com/messaging/thread/{THREAD}/?focus=true#top"),
    );
    let ThreadUrlResolution::Resolved {
        conversation_id,
        participant,
    } = &resolved
    else {
        panic!("{resolved:?}");
    };
    assert_eq!(conversation_id, THREAD);
    assert_eq!(participant.name, "Dana Example");
    assert_eq!(participant.urn, "person:1");
    // The archive never records a public profile URL; saying so is the point.
    assert!(resolved.detail().contains("profile URL"), "{resolved:?}");
    assert!(!resolved.detail().contains(&redirect), "{resolved:?}");
    // The dead session is still worth reporting, but it cannot unsay an answer
    // the archive already has, so no reconnect note is attached.
    let advice = session_advice(&store, OWNER, &resolved).unwrap().unwrap();
    assert_eq!(advice.reconnect_command, session_guard::LOGIN_COMMAND);
    assert_eq!(advice.since_ms, 1_700_000_000_000);
    assert_eq!(advice.note, None);

    let receipt: serde_json::Value =
        serde_json::from_str(&resolve_thread_url_report(&store, OWNER, &url(), true).unwrap())
            .unwrap();
    assert_eq!(receipt["state"], "resolved");
    assert_eq!(receipt["conversation_id"], THREAD);
    assert_eq!(receipt["account_id"], format!("linkedin:{OWNER}"));
    assert_eq!(receipt["participant"]["name"], "Dana Example");
    assert_eq!(receipt["participant"]["member_urn"], "person:1");
    assert!(receipt["participant"]["profile_url"].is_null());
    assert_eq!(receipt["network_requests"], 0);
    assert_eq!(receipt["live_lookup_enabled"], false);
    assert_eq!(
        receipt["session"]["reconnect_command"],
        session_guard::LOGIN_COMMAND
    );
    assert!(receipt["session"]["note"].is_null(), "{receipt}");
}

#[test]
fn only_https_linkedin_messaging_thread_urls_are_accepted() {
    assert_eq!(
        parse_thread_url("https://linkedin.com/messaging/thread/abc/?x=1").unwrap(),
        "abc"
    );
    assert_eq!(
        parse_thread_url("https://www.linkedin.com/messaging/thread/abc").unwrap(),
        "abc"
    );
    for (rejected, expected) in [
        ("http://www.linkedin.com/messaging/thread/abc/", ThreadUrlError::Host),
        ("https://linkedin.example/messaging/thread/abc/", ThreadUrlError::Host),
        ("https://www.linkedin.com.example/messaging/thread/abc/", ThreadUrlError::Host),
        ("not a url", ThreadUrlError::Host),
        ("https://www.linkedin.com/in/dana-example/", ThreadUrlError::Path),
        ("https://www.linkedin.com/messaging/thread/abc/extra", ThreadUrlError::Path),
        ("https://www.linkedin.com/messaging//thread/abc/", ThreadUrlError::Path),
        ("https://www.linkedin.com/messaging/thread/", ThreadUrlError::Path),
        ("https://www.linkedin.com/messaging/thread/a%2Fb/", ThreadUrlError::ConversationId),
    ] {
        assert_eq!(parse_thread_url(rejected), Err(expected), "{rejected}");
    }
}

#[test]
fn resolution_is_scoped_to_the_selected_account() {
    let store = Store::open(":memory:").unwrap();
    sync(
        &store,
        OTHER,
        false,
        vec![participant("person:1", "Dana Example")],
    );
    assert!(matches!(
        resolve(&store, OWNER, &url()),
        ThreadUrlResolution::MappingMissing { .. }
    ));
    assert!(matches!(
        resolve(&store, OTHER, &url()),
        ThreadUrlResolution::Resolved { .. }
    ));
}

#[test]
fn group_and_peerless_threads_cannot_name_one_person() {
    let dana = participant("person:1", "Dana Example");
    let rae = participant("person:2", "Rae Example");
    for (group, peers, expected) in [
        (true, vec![dana.clone()], (1_usize, true)),
        (false, vec![dana, rae], (2, false)),
        (false, Vec::new(), (0, false)),
    ] {
        let store = Store::open(":memory:").unwrap();
        sync(&store, OWNER, group, peers);
        let resolved = resolve(&store, OWNER, &url());
        let ThreadUrlResolution::Ambiguous {
            peers, group: flag, ..
        } = resolved
        else {
            panic!("{resolved:?}");
        };
        assert_eq!((peers, flag), expected);
    }
}

#[test]
fn an_unsynced_thread_explains_itself_and_offers_the_reconnect_command() {
    let store = Store::open(":memory:").unwrap();
    let redirect = ArchiveError::Redirect.to_string();
    session_guard::record_failure(&store, OWNER, &redirect, 1_700_000_000_000).unwrap();

    let missing = resolve(&store, OWNER, &url());
    assert_eq!(missing.state(), "archive_mapping_missing");
    assert!(missing.detail().contains("stored LinkedIn archive"), "{missing:?}");
    // A stale session must never surface to the owner as a redirect or
    // request-compatibility error.
    assert!(!missing.detail().contains(&redirect), "{missing:?}");
    let advice = session_advice(&store, OWNER, &missing).unwrap().unwrap();
    assert_eq!(advice.note, Some(RECONNECT_NOTE));
    // A healthy account is told nothing about reconnecting.
    assert!(session_advice(&store, OTHER, &missing).unwrap().is_none());

    let text = resolve_thread_url_report(&store, OWNER, &url(), false).unwrap();
    assert!(text.contains("state: archive_mapping_missing"), "{text}");
    assert!(text.contains(session_guard::LOGIN_COMMAND), "{text}");
    assert!(text.contains(RECONNECT_NOTE), "{text}");
    assert!(text.contains("network_requests=0"), "{text}");
    assert!(!text.contains(&redirect), "{text}");

    let unsupported = resolve(&store, OWNER, "https://www.linkedin.com/feed/");
    assert_eq!(unsupported.state(), "unsupported_url");
    assert!(unsupported.detail().contains("/messaging/thread/"));
}
