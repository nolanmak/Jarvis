//! Offline resolution of a messaging-thread URL. No HTTP client is built here,
//! so a passing test is also proof the lookup is archive-only.
use augmentagent_channel_linkedin::{
    archive::{ArchiveError, Conversation, Page, Participant},
    archive_store::{checkpoint, commit_conversations},
    session_guard,
    thread_url_resolution::{
        parse_thread_url, resolve_thread_url, resolve_thread_url_report, session_advice,
        ThreadUrlError, ThreadUrlResolution, OPT_IN_FLAG, RECONNECT_NOTE,
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

fn dana() -> Vec<Participant> {
    vec![participant("person:1", "Dana Example")]
}

fn url() -> String {
    format!("https://www.linkedin.com/messaging/thread/{THREAD}/")
}

/// Sync under an explicit conversation key, because a mailbox may have
/// recorded the composite urn rather than the bare thread id. The mailbox
/// member urn lands as the account and as a participant, as sync records it.
fn sync(store: &Store, account: &str, urn: &str, group: bool, peers: Vec<Participant>) {
    let cp = checkpoint(store, account, "PRIMARY_INBOX").unwrap();
    let items = vec![Conversation {
        urn: urn.into(),
        title: "Thread".into(),
        group,
        last_activity_ms: 100,
        participants: std::iter::once(participant(account, "Owner"))
            .chain(peers)
            .collect(),
    }];
    let page = Page { items, next: None };
    assert!(commit_conversations(store, account, "PRIMARY_INBOX", &cp, &page, 10_000).unwrap());
}

/// Every case but the opt-in test resolves with the surface switched on.
fn resolve(store: &Store, account: &str, url: &str) -> ThreadUrlResolution {
    resolve_thread_url(store, account, url, true).unwrap()
}

/// The reported case: the live session is rejected, yet the pasted thread URL
/// still names the person, because resolution reads the archive instead.
#[test]
fn a_stale_session_still_resolves_an_archived_thread_url_to_its_participant() {
    let store = Store::open(":memory:").unwrap();
    let redirect = ArchiveError::Redirect.to_string();
    session_guard::record_failure(&store, OWNER, &redirect, 1_700_000_000_000).unwrap();
    sync(&store, OWNER, THREAD, false, dana());

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

    let receipt: serde_json::Value = serde_json::from_str(
        &resolve_thread_url_report(&store, OWNER, &url(), true, true).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["state"], "resolved");
    assert_eq!(receipt["conversation_id"], THREAD);
    assert_eq!(receipt["account_id"], format!("linkedin:{OWNER}"));
    assert_eq!(receipt["participant"]["name"], "Dana Example");
    assert_eq!(receipt["participant"]["member_urn"], "person:1");
    assert!(receipt["participant"]["profile_url"].is_null(), "{receipt}");
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

/// The surface is opt-in, so a thread the archive could name stays unnamed
/// until the owner asks for it, and says which flag to pass.
#[test]
fn resolution_is_off_until_it_is_explicitly_enabled() {
    let store = Store::open(":memory:").unwrap();
    session_guard::record_failure(&store, OWNER, "redirected", 1_700_000_000_000).unwrap();
    sync(&store, OWNER, THREAD, false, dana());

    let disabled = resolve_thread_url(&store, OWNER, &url(), false).unwrap();
    assert_eq!(disabled.state(), "resolution_disabled");
    assert!(disabled.detail().contains(OPT_IN_FLAG), "{disabled:?}");
    // An opt-out is not an identification: the archived name is withheld.
    assert!(!disabled.detail().contains("Dana Example"), "{disabled:?}");
    assert_eq!(disabled.conversation_id(), None);
    // An opt-out is not an archive gap, so no reconnect note is attached.
    let advice = session_advice(&store, OWNER, &disabled).unwrap();
    assert_eq!(advice.and_then(|advice| advice.note), None);

    let receipt: serde_json::Value = serde_json::from_str(
        &resolve_thread_url_report(&store, OWNER, &url(), true, false).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["state"], "resolution_disabled");
    assert!(receipt["participant"].is_null(), "{receipt}");
    assert!(receipt["conversation_id"].is_null(), "{receipt}");
    assert_eq!(receipt["network_requests"], 0);
}

/// A link carries only the trailing thread id, while sync stores whatever
/// `entityUrn` the mailbox returned. The composite form must still resolve, or
/// every archived thread reads as missing.
#[test]
fn a_composite_conversation_urn_resolves_from_the_thread_id_in_the_link() {
    let store = Store::open(":memory:").unwrap();
    let urn = format!("urn:li:msgConversation:({OWNER},{THREAD})");
    sync(&store, OWNER, &urn, false, dana());

    let resolved = resolve(&store, OWNER, &url());
    let ThreadUrlResolution::Resolved {
        conversation_id,
        participant,
    } = &resolved
    else {
        panic!("{resolved:?}");
    };
    assert_eq!(conversation_id, THREAD);
    assert_eq!(participant.urn, "person:1");

    // Matching the trailing element widens the key, never the account scope,
    // and never reaches a thread id the mailbox does not hold.
    for (account, link) in [
        (OTHER, url()),
        (
            OWNER,
            "https://www.linkedin.com/messaging/thread/2-other-thread-id==/".into(),
        ),
    ] {
        assert_eq!(
            resolve(&store, account, &link).state(),
            "archive_mapping_missing"
        );
    }
}

#[test]
fn group_and_peerless_threads_cannot_name_one_person() {
    let rae = participant("person:2", "Rae Example");
    for (group, peers, expected) in [
        (true, dana(), (1_usize, true)),
        (false, [dana(), vec![rae]].concat(), (2, false)),
        (false, Vec::new(), (0, false)),
    ] {
        let store = Store::open(":memory:").unwrap();
        sync(&store, OWNER, THREAD, group, peers);
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
    let advice = session_advice(&store, OWNER, &missing).unwrap().unwrap();
    assert_eq!(advice.note, Some(RECONNECT_NOTE));
    // A healthy account is told nothing about reconnecting.
    assert!(session_advice(&store, OTHER, &missing).unwrap().is_none());

    let text = resolve_thread_url_report(&store, OWNER, &url(), false, true).unwrap();
    assert!(text.contains("state: archive_mapping_missing"), "{text}");
    assert!(text.contains("stored LinkedIn archive"), "{text}");
    assert!(text.contains(session_guard::LOGIN_COMMAND), "{text}");
    assert!(text.contains(RECONNECT_NOTE), "{text}");
    assert!(text.contains("network_requests=0"), "{text}");
    // A stale session must never surface to the owner as a redirect or
    // request-compatibility error.
    assert!(!text.contains(&redirect), "{text}");

    let unsupported = resolve(&store, OWNER, "https://www.linkedin.com/feed/");
    assert_eq!(unsupported.state(), "unsupported_url");
    assert!(unsupported.detail().contains("/messaging/thread/"));
}
