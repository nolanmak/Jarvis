//! #1286 — binding the Slack owner and choosing the control channel, against
//! a temporary store, an in-memory credential store and a fake Slack.
//! Nothing touches a real Keychain or slack.com; identifiers and tokens are
//! synthetic.

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_auth::MemoryCredentialStore;
use augmentagent_channel_slack::app::{
    parse_app_token, parse_bot_token, AppTokenCheck, SlackAppConnector, SlackAppCredentials,
    SlackAppError, SlackAppStore,
};
use augmentagent_channel_slack::owner::{AuthDecision, IgnoreReason, RejectReason};
use augmentagent_channel_slack::owner_setup::{
    self, bot_identity, DirectConversation, OwnerIneligibility,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::{parse_envelope, Envelope, EventEnvelope};
use augmentagent_channel_slack::transport::web::{
    AuthTest, ConversationInfo, RecordedCall, RecordingSlackWebApi, SlackWebApi, UserInfo,
};
use augmentagent_channel_slack::transport::{AppLevelToken, BotToken};
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::Store;
use serde_json::{json, Value};

const TEAM: &str = "T00000001";
const OTHER_TEAM: &str = "T00000002";
const ENTERPRISE: &str = "E00000001";
const OWNER: &str = "U00000001";
const GUEST: &str = "U00000003";
const BOT_USER: &str = "U0000000B";
const DM: &str = "D00000001";
const CONTROL: &str = "C00000001";
const NOW_MS: i64 = 1_700_000_000_000;

struct FakeConnector {
    api: Arc<RecordingSlackWebApi>,
}

#[async_trait]
impl SlackAppConnector for FakeConnector {
    fn web_api(&self, _bot: &BotToken) -> Result<Arc<dyn SlackWebApi>, SlackAppError> {
        Ok(self.api.clone())
    }
    async fn probe_app_token(&self, _app: &AppLevelToken) -> Result<AppTokenCheck, SlackAppError> {
        Ok(AppTokenCheck {
            app_id: Some("A00000001".into()),
        })
    }
}

fn who(team: &str, enterprise: Option<&str>) -> AuthTest {
    AuthTest {
        team_id: team.into(),
        team: Some("Example Test".into()),
        url: None,
        user_id: BOT_USER.into(),
        user: Some("jarvis".into()),
        bot_id: Some("B00000001".into()),
        app_id: Some("A00000001".into()),
        enterprise_id: enterprise.map(str::to_string),
        scopes: None,
    }
}

fn user(id: &str, raw: Value) -> UserInfo {
    UserInfo {
        id: id.into(),
        name: Some("owner".into()),
        real_name: Some("Test Owner".into()),
        display_name: None,
        is_bot: raw.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
        tz: None,
        raw,
    }
}

fn member(id: &str) -> Value {
    json!({"id": id, "team_id": TEAM, "name": "owner", "deleted": false,
           "is_bot": false, "is_app_user": false, "is_restricted": false,
           "is_ultra_restricted": false})
}

fn with(mut v: Value, k: &str, x: Value) -> Value {
    v.as_object_mut().unwrap().insert(k.into(), x);
    v
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    creds: SlackAppStore,
    api: Arc<RecordingSlackWebApi>,
    connector: FakeConnector,
}

fn credentials() -> SlackAppCredentials {
    SlackAppCredentials {
        team_id: TEAM.into(),
        team_name: Some("Example Test".into()),
        team_url: None,
        bot_user_id: BOT_USER.into(),
        bot_user_name: Some("jarvis".into()),
        bot_id: Some("B00000001".into()),
        app_id: Some("A00000001".into()),
        scopes: None,
        installed_at: 1_700_000_000,
        verified_at: None,
        rotated_at: None,
        app_token: parse_app_token("xapp-test-000").unwrap(),
        bot_token: parse_bot_token("xoxb-test-000").unwrap(),
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let creds = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
    creds.save(&credentials()).unwrap();
    let api = Arc::new(RecordingSlackWebApi::default());
    api.set_auth_test(who(TEAM, None));
    api.add_user(user(OWNER, member(OWNER)));
    api.set_direct_conversation(OWNER, DM);
    let connector = FakeConnector { api: api.clone() };
    Fixture {
        _dir: dir,
        store,
        creds,
        api,
        connector,
    }
}

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

async fn bind(f: &Fixture, user_id: &str) -> Result<owner_setup::OwnerBindOutcome, SlackAppError> {
    owner_setup::bind_owner(&f.store, &f.creds, &f.connector, None, user_id, NOW_MS).await
}

// ---------------------------------------------------------------------------
// bind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bind_verifies_the_user_and_records_owner_and_dm() {
    let f = fixture();
    let out = bind(&f, OWNER).await.unwrap();
    assert_eq!(out.team_id, TEAM);
    assert_eq!(out.owner_user_id, OWNER);
    assert_eq!(out.owner_name.as_deref(), Some("owner"));
    assert_eq!(
        out.direct_conversation,
        DirectConversation::Recorded(DM.into())
    );
    assert_eq!(out.replaced_owner, None);

    let binding = f
        .store
        .surface_owner_binding(&workspace().account())
        .unwrap()
        .expect("bound");
    assert_eq!(binding.owner, workspace().owner(OWNER).unwrap());
    assert_eq!(binding.confirmed_at_ms, NOW_MS);
    assert_eq!(
        binding.direct_conversation(),
        Some(&workspace().conversation(DM, None).unwrap())
    );
    let calls = f.api.calls();
    assert!(calls.contains(&RecordedCall::AuthTest));
    assert!(calls.contains(&RecordedCall::UserInfo {
        user_id: OWNER.into()
    }));
    assert!(calls.contains(&RecordedCall::OpenDirectConversation {
        user_id: OWNER.into()
    }));
}

#[tokio::test]
async fn bind_keeps_any_dm_rule_when_the_dm_cannot_be_opened() {
    let f = fixture();
    f.api.fail_direct_conversation(OWNER, "missing_scope");
    let out = bind(&f, OWNER).await.unwrap();
    assert_eq!(
        out.direct_conversation,
        DirectConversation::Unresolved("missing_scope".into())
    );
    let binding = f
        .store
        .surface_owner_binding(&workspace().account())
        .unwrap()
        .unwrap();
    assert_eq!(binding.direct_conversation(), None);
}

/// Member ID, fake setup, expected refusal (`None` = not found).
type UserCase = (&'static str, Box<dyn Fn(&Fixture)>, Option<OwnerIneligibility>);

#[tokio::test]
async fn bind_refuses_unknown_guest_bot_deactivated_and_foreign_users() {
    let cases: Vec<UserCase> = vec![
        (
            "U00000404",
            Box::new(|f: &Fixture| f.api.fail_user("U00000404", "user_not_found")),
            None,
        ),
        (
            GUEST,
            Box::new(|f: &Fixture| {
                f.api.add_user(user(
                    GUEST,
                    with(member(GUEST), "is_restricted", json!(true)),
                ))
            }),
            Some(OwnerIneligibility::Guest),
        ),
        (
            GUEST,
            Box::new(|f: &Fixture| {
                f.api.add_user(user(
                    GUEST,
                    with(member(GUEST), "is_ultra_restricted", json!(true)),
                ))
            }),
            Some(OwnerIneligibility::Guest),
        ),
        (
            "U00000004",
            Box::new(|f: &Fixture| {
                f.api.add_user(user(
                    "U00000004",
                    with(member("U00000004"), "is_bot", json!(true)),
                ))
            }),
            Some(OwnerIneligibility::Bot),
        ),
        (
            BOT_USER,
            Box::new(|f: &Fixture| f.api.add_user(user(BOT_USER, member(BOT_USER)))),
            Some(OwnerIneligibility::Bot),
        ),
        (
            "U00000005",
            Box::new(|f: &Fixture| {
                f.api.add_user(user(
                    "U00000005",
                    with(member("U00000005"), "deleted", json!(true)),
                ))
            }),
            Some(OwnerIneligibility::Deactivated),
        ),
        (
            "U00000006",
            Box::new(|f: &Fixture| {
                f.api.add_user(user(
                    "U00000006",
                    with(member("U00000006"), "team_id", json!(OTHER_TEAM)),
                ))
            }),
            Some(OwnerIneligibility::OtherWorkspace),
        ),
        (
            "U00000007",
            Box::new(|f: &Fixture| {
                f.api.add_user(user(
                    "U00000007",
                    with(member("U00000007"), "is_stranger", json!(true)),
                ))
            }),
            Some(OwnerIneligibility::External),
        ),
        (
            // users.info without a team cannot prove membership.
            "U00000008",
            Box::new(|f: &Fixture| {
                f.api
                    .add_user(user("U00000008", json!({"id": "U00000008"})))
            }),
            Some(OwnerIneligibility::Unverifiable),
        ),
    ];
    for (id, setup, expected) in cases {
        let f = fixture();
        setup(&f);
        let err = bind(&f, id).await.unwrap_err();
        match (&err, expected) {
            (SlackAppError::OwnerNotFound { user_id }, None) => assert_eq!(user_id, id),
            (SlackAppError::OwnerIneligible { user_id, reason }, Some(want)) => {
                assert_eq!(user_id, id);
                assert_eq!(*reason, want, "{id}");
            }
            other => panic!("{id}: unexpected {other:?}"),
        }
        assert!(!err.recovery().is_empty());
        assert_eq!(
            f.store
                .surface_owner_binding(&workspace().account())
                .unwrap(),
            None,
            "{id}: nothing may be bound on failure"
        );
    }
}

#[tokio::test]
async fn bind_refuses_malformed_ids_without_calling_slack() {
    let f = fixture();
    let err = bind(&f, "U0000 0001").await.unwrap_err();
    assert_eq!(err.code(), "invalid_id");
    assert!(f.api.calls().is_empty());
}

#[tokio::test]
async fn bind_requires_an_installed_app_matching_the_token_workspace() {
    let f = fixture();
    let err = owner_setup::bind_owner(
        &f.store,
        &f.creds,
        &f.connector,
        Some(OTHER_TEAM),
        OWNER,
        NOW_MS,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "not_installed");

    let empty = SlackAppStore::new(Arc::new(MemoryCredentialStore::default()));
    let err = owner_setup::bind_owner(&f.store, &empty, &f.connector, None, OWNER, NOW_MS)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "nothing_installed");

    // The stored token now answers for another workspace.
    f.api.set_auth_test(who(OTHER_TEAM, None));
    let err = bind(&f, OWNER).await.unwrap_err();
    assert_eq!(err.code(), "wrong_workspace");
    assert_eq!(
        f.store
            .surface_owner_binding(&workspace().account())
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn enterprise_grid_install_binds_under_the_enterprise_account() {
    let f = fixture();
    f.api.set_auth_test(who(TEAM, Some(ENTERPRISE)));
    // Grid user record: home team may differ; enterprise membership lists ours.
    f.api.add_user(user(
        "W00000001",
        json!({"id": "W00000001", "team_id": ENTERPRISE, "deleted": false,
               "enterprise_user": {"enterprise_id": ENTERPRISE, "teams": [OTHER_TEAM, TEAM]}}),
    ));
    let out = bind(&f, "W00000001").await.unwrap();
    assert_eq!(out.enterprise_id.as_deref(), Some(ENTERPRISE));
    let grid = SlackWorkspace::new(TEAM, Some(ENTERPRISE)).unwrap();
    assert!(f
        .store
        .surface_owner_binding(&grid.account())
        .unwrap()
        .is_some());
    assert_eq!(
        f.store
            .surface_owner_binding(&workspace().account())
            .unwrap(),
        None
    );
    // A grid member who is not in this team is refused.
    f.api.add_user(user(
        "W00000002",
        json!({"id": "W00000002", "team_id": ENTERPRISE, "deleted": false,
               "enterprise_user": {"enterprise_id": ENTERPRISE, "teams": [OTHER_TEAM]}}),
    ));
    let err = bind(&f, "W00000002").await.unwrap_err();
    assert!(
        matches!(
            err,
            SlackAppError::OwnerIneligible {
                reason: OwnerIneligibility::OtherWorkspace,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn rebinding_another_owner_reports_it_and_drops_control_conversations() {
    let f = fixture();
    bind(&f, OWNER).await.unwrap();
    f.api
        .add_conversation(channel(CONTROL, private_channel(CONTROL)));
    owner_setup::set_control_channel(&f.store, &f.creds, &f.connector, None, CONTROL, NOW_MS)
        .await
        .unwrap();
    f.api.add_user(user("U00000009", member("U00000009")));
    let out = bind(&f, "U00000009").await.unwrap();
    assert_eq!(out.replaced_owner.as_deref(), Some(OWNER));
    let binding = f
        .store
        .surface_owner_binding(&workspace().account())
        .unwrap()
        .unwrap();
    assert_eq!(binding.control_channel(), None);
    assert_eq!(
        binding.direct_conversation().map(|c| c.conversation_id()),
        Some("D00000009")
    );
}

// ---------------------------------------------------------------------------
// show / unbind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_binding_bot_identity_and_rejections_locally() {
    let f = fixture();
    let status = owner_setup::owner_status(&f.store, &f.creds, None).unwrap();
    assert_eq!(status.team_id, TEAM);
    assert!(status.binding.is_none());
    bind(&f, OWNER).await.unwrap();
    f.api.clear_calls();
    let status = owner_setup::owner_status(&f.store, &f.creds, None).unwrap();
    assert_eq!(status.binding.unwrap().owner.sender_id(), OWNER);
    assert_eq!(
        status.bot.as_ref().unwrap().bot_user_id.as_deref(),
        Some(BOT_USER)
    );
    assert_eq!(status.rejections, 0);
    assert!(f.api.calls().is_empty(), "show is local");
}

#[tokio::test]
async fn unbind_removes_the_binding_and_is_idempotent() {
    let f = fixture();
    bind(&f, OWNER).await.unwrap();
    assert!(
        owner_setup::unbind_owner(&f.store, &f.creds, None)
            .unwrap()
            .1
    );
    assert!(
        !owner_setup::unbind_owner(&f.store, &f.creds, None)
            .unwrap()
            .1
    );
    assert_eq!(
        f.store
            .surface_owner_binding(&workspace().account())
            .unwrap(),
        None
    );
    // Works with an explicit team even after the app itself was removed.
    bind(&f, OWNER).await.unwrap();
    f.creds.remove(TEAM).unwrap();
    assert!(
        owner_setup::unbind_owner(&f.store, &f.creds, Some(TEAM))
            .unwrap()
            .1
    );
}

// ---------------------------------------------------------------------------
// control channel
// ---------------------------------------------------------------------------

fn channel(id: &str, raw: Value) -> ConversationInfo {
    let flag = |k: &str| raw.get(k).and_then(Value::as_bool).unwrap_or(false);
    ConversationInfo {
        id: id.into(),
        name: Some("jarvis-control".into()),
        is_channel: flag("is_channel"),
        is_im: flag("is_im"),
        is_mpim: flag("is_mpim"),
        is_private: flag("is_private"),
        user: None,
        raw,
    }
}

fn private_channel(id: &str) -> Value {
    json!({"id": id, "is_channel": true, "is_group": true, "is_private": true,
           "is_member": true, "is_archived": false, "is_ext_shared": false})
}

#[tokio::test]
async fn control_channel_must_be_a_private_unshared_channel_the_app_is_in() {
    let cases = [
        (
            "C00000002",
            with(private_channel("C00000002"), "is_private", json!(false)),
            OwnerIneligibility::PublicChannel,
        ),
        (
            "C00000003",
            with(private_channel("C00000003"), "is_ext_shared", json!(true)),
            OwnerIneligibility::ExternallyShared,
        ),
        (
            "C00000004",
            with(
                private_channel("C00000004"),
                "is_pending_ext_shared",
                json!(true),
            ),
            OwnerIneligibility::ExternallyShared,
        ),
        (
            "C00000005",
            with(private_channel("C00000005"), "is_archived", json!(true)),
            OwnerIneligibility::Archived,
        ),
        (
            "C00000006",
            with(private_channel("C00000006"), "is_member", json!(false)),
            OwnerIneligibility::AppNotMember,
        ),
        (
            "D00000002",
            json!({"id": "D00000002", "is_im": true, "is_member": true}),
            OwnerIneligibility::NotAChannel,
        ),
        (
            "G00000001",
            json!({"id": "G00000001", "is_mpim": true, "is_private": true, "is_member": true}),
            OwnerIneligibility::NotAChannel,
        ),
    ];
    for (id, raw, want) in cases {
        let f = fixture();
        bind(&f, OWNER).await.unwrap();
        f.api.add_conversation(channel(id, raw));
        let err =
            owner_setup::set_control_channel(&f.store, &f.creds, &f.connector, None, id, NOW_MS)
                .await
                .unwrap_err();
        match err {
            SlackAppError::ControlChannelIneligible { channel_id, reason } => {
                assert_eq!(channel_id, id);
                assert_eq!(reason, want, "{id}");
            }
            other => panic!("{id}: unexpected {other:?}"),
        }
        let binding = f
            .store
            .surface_owner_binding(&workspace().account())
            .unwrap()
            .unwrap();
        assert_eq!(binding.control_channel(), None, "{id}");
    }
}

#[tokio::test]
async fn control_channel_set_and_remove() {
    let f = fixture();
    let err =
        owner_setup::set_control_channel(&f.store, &f.creds, &f.connector, None, CONTROL, NOW_MS)
            .await
            .unwrap_err();
    assert_eq!(err.code(), "owner_not_bound");

    bind(&f, OWNER).await.unwrap();
    f.api
        .add_conversation(channel(CONTROL, private_channel(CONTROL)));
    let conv =
        owner_setup::set_control_channel(&f.store, &f.creds, &f.connector, None, CONTROL, NOW_MS)
            .await
            .unwrap();
    assert_eq!(conv, workspace().conversation(CONTROL, None).unwrap());
    let binding = f
        .store
        .surface_owner_binding(&workspace().account())
        .unwrap()
        .unwrap();
    assert_eq!(binding.control_channel(), Some(&conv));
    assert_eq!(
        binding
            .control
            .iter()
            .filter(|c| c.kind == ControlConversationKind::Channel)
            .count(),
        1
    );

    f.api.push_error(
        augmentagent_channel_slack::transport::web::WebApiError::Slack {
            error: "channel_not_found".into(),
            warning: None,
        },
    );
    let err = owner_setup::set_control_channel(
        &f.store,
        &f.creds,
        &f.connector,
        None,
        "C00000404",
        NOW_MS,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "channel_not_found");

    assert!(
        owner_setup::remove_control_channel(&f.store, &f.creds, None)
            .unwrap()
            .1
    );
    assert!(
        !owner_setup::remove_control_channel(&f.store, &f.creds, None)
            .unwrap()
            .1
    );
}

// ---------------------------------------------------------------------------
// what serve (#1287) loads
// ---------------------------------------------------------------------------

fn envelope(event: Value) -> EventEnvelope {
    let frame = json!({"type": "events_api", "envelope_id": "env-1",
        "payload": {"type": "event_callback", "team_id": TEAM, "event_id": "Ev00000001", "event": event}});
    match parse_envelope(&frame.to_string()).unwrap() {
        Envelope::Event(e) => *e,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn load_authorizer_uses_bindings_and_the_installed_bot_identity() {
    let f = fixture();
    bind(&f, OWNER).await.unwrap();
    let auth = owner_setup::load_authorizer(&f.store, &f.creds).unwrap();
    let owner_dm = json!({"type": "message", "channel": DM, "channel_type": "im",
        "user": OWNER, "team": TEAM, "text": "hi", "ts": "1700000000.000100"});
    assert!(matches!(
        auth.authorize(&envelope(owner_dm)),
        AuthDecision::Owner(_)
    ));
    // The bot user from the install record, with no bot_id on the event.
    let echo = json!({"type": "message", "channel": DM, "channel_type": "im",
        "user": BOT_USER, "text": "answer", "ts": "1700000000.000200"});
    assert_eq!(
        auth.authorize(&envelope(echo)),
        AuthDecision::Ignore(IgnoreReason::OwnMessage)
    );
    let stranger = json!({"type": "message", "channel": "D00000002", "channel_type": "im",
        "user": "U00000002", "text": "hi", "ts": "1700000000.000300"});
    assert!(matches!(
        auth.authorize(&envelope(stranger)),
        AuthDecision::Reject(r) if r.reason == RejectReason::NotOwner
    ));
    assert_eq!(
        bot_identity(&credentials()).bot_id.as_deref(),
        Some("B00000001")
    );
}
