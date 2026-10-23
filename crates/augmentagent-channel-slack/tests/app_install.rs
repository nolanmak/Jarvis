//! #1284 — interactive Slack app credential lifecycle against an in-memory
//! credential store and a fake Slack. Nothing touches a real Keychain,
//! keyring or slack.com. Tokens are synthetic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_auth::{CredentialStore, MemoryCredentialStore};
use augmentagent_channel_slack::app::{
    self, parse_app_token, parse_bot_token, AppTokenCheck, SlackAppConnector, SlackAppError,
    SlackAppStore, TokenKind, APP_CREDENTIAL_PLATFORM, REQUIRED_BOT_SCOPES,
};
use augmentagent_channel_slack::transport::web::{
    AuthTest, RecordingSlackWebApi, SlackWebApi, WebApiError,
};
use augmentagent_channel_slack::transport::{AppLevelToken, BotToken};
use augmentagent_channel_slack::SlackAuth;

const APP: &str = "xapp-test-000";
const APP2: &str = "xapp-test-001";
const BOT: &str = "xoxb-test-000";
const BOT2: &str = "xoxb-test-001";
const BOT_OTHER_TEAM: &str = "xoxb-test-999";
const BOT_REVOKED: &str = "xoxb-test-bad";
const BOT_FEW_SCOPES: &str = "xoxb-test-few";
const APP_REVOKED: &str = "xapp-test-revoked";
const TEAM: &str = "T00000001";
const NOW: u64 = 1_700_000_000;

fn all_scopes() -> Vec<String> {
    REQUIRED_BOT_SCOPES.iter().map(|s| s.to_string()).collect()
}

fn who(team: &str, scopes: Option<Vec<String>>) -> AuthTest {
    AuthTest {
        team_id: team.into(),
        team: Some("Example Test".into()),
        url: None,
        user_id: "U00000001".into(),
        user: Some("jarvis".into()),
        bot_id: Some("B00000001".into()),
        app_id: None,
        enterprise_id: None,
        scopes,
    }
}

/// Fake Slack: one scripted `auth.test` per bot token, one result per
/// app-level token. Records which tokens were used.
#[derive(Default)]
struct FakeSlack {
    bots: Mutex<HashMap<String, Result<AuthTest, String>>>,
    apps: Mutex<HashMap<String, Result<AppTokenCheck, String>>>,
    used: Mutex<Vec<String>>,
}

impl FakeSlack {
    fn standard() -> Self {
        let f = FakeSlack::default();
        {
            let mut b = f.bots.lock().unwrap();
            b.insert(BOT.into(), Ok(who(TEAM, Some(all_scopes()))));
            b.insert(BOT2.into(), Ok(who(TEAM, Some(all_scopes()))));
            b.insert(
                BOT_OTHER_TEAM.into(),
                Ok(who("T00000002", Some(all_scopes()))),
            );
            b.insert(BOT_REVOKED.into(), Err("invalid_auth".into()));
            b.insert(
                BOT_FEW_SCOPES.into(),
                Ok(who(
                    TEAM,
                    Some(vec!["chat:write".into(), "users:read".into()]),
                )),
            );
            let mut a = f.apps.lock().unwrap();
            let ok = Ok(AppTokenCheck {
                app_id: Some("A00000001".into()),
            });
            a.insert(APP.into(), ok.clone());
            a.insert(APP2.into(), ok);
            a.insert(APP_REVOKED.into(), Err("invalid_auth".into()));
        }
        f
    }
}

#[async_trait]
impl SlackAppConnector for FakeSlack {
    fn web_api(&self, bot: &BotToken) -> Result<Arc<dyn SlackWebApi>, SlackAppError> {
        self.used.lock().unwrap().push(bot.expose_secret().into());
        let api = RecordingSlackWebApi::default();
        match self.bots.lock().unwrap().get(bot.expose_secret()) {
            Some(Ok(w)) => api.set_auth_test(w.clone()),
            Some(Err(e)) => api.push_error(WebApiError::Slack {
                error: e.clone(),
                warning: None,
            }),
            None => api.push_error(WebApiError::Slack {
                error: "invalid_auth".into(),
                warning: None,
            }),
        }
        Ok(Arc::new(api))
    }

    async fn probe_app_token(&self, app: &AppLevelToken) -> Result<AppTokenCheck, SlackAppError> {
        self.used.lock().unwrap().push(app.expose_secret().into());
        match self.apps.lock().unwrap().get(app.expose_secret()) {
            Some(Ok(c)) => Ok(c.clone()),
            Some(Err(e)) => Err(SlackAppError::InvalidToken {
                which: TokenKind::AppLevel,
                slack_error: e.clone(),
            }),
            None => Err(SlackAppError::InvalidToken {
                which: TokenKind::AppLevel,
                slack_error: "invalid_auth".into(),
            }),
        }
    }
}

fn store() -> (MemoryCredentialStore, SlackAppStore) {
    let mem = MemoryCredentialStore::default();
    let s = SlackAppStore::new(Arc::new(mem.clone()));
    (mem, s)
}

fn app_tok(s: &str) -> AppLevelToken {
    parse_app_token(s).unwrap()
}
fn bot_tok(s: &str) -> BotToken {
    parse_bot_token(s).unwrap()
}

/// No stored payload other than the credential slot itself may contain token
/// bytes, and no rendered output may either.
fn assert_no_tokens(text: &str) {
    for t in [
        APP,
        APP2,
        BOT,
        BOT2,
        BOT_OTHER_TEAM,
        BOT_REVOKED,
        "xoxb-",
        "xapp-",
    ] {
        assert!(!text.contains(t), "token bytes {t:?} leaked into: {text}");
    }
}

#[tokio::test]
async fn install_with_valid_tokens_stores_both_and_reports_identity() {
    let (mem, s) = store();
    let slack = FakeSlack::standard();
    let out = app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    assert!(!out.replaced);
    let sum = &out.summary;
    assert_eq!(sum.team_id, TEAM);
    assert_eq!(sum.team_name.as_deref(), Some("Example Test"));
    assert_eq!(sum.bot_user_id, "U00000001");
    assert_eq!(sum.bot_id.as_deref(), Some("B00000001"));
    assert_eq!(sum.app_id.as_deref(), Some("A00000001"));
    assert_eq!(sum.missing_scopes, Some(vec![]));
    assert_eq!(sum.installed_at, NOW);
    assert_eq!(sum.verified_at, Some(NOW));

    let loaded = s.load(TEAM).unwrap().expect("stored");
    assert_eq!(loaded.app_token.expose_secret(), APP);
    assert_eq!(loaded.bot_token.expose_secret(), BOT);
    assert_eq!(s.teams().unwrap(), vec![TEAM.to_string()]);
    assert!(mem.exists(APP_CREDENTIAL_PLATFORM, TEAM));

    assert_no_tokens(&serde_json::to_string(&out.summary).unwrap());
    assert_no_tokens(&format!("{loaded:?} {out:?}"));
}

#[tokio::test]
async fn install_rejects_missing_scopes_by_name_and_stores_nothing() {
    let (mem, s) = store();
    let slack = FakeSlack::standard();
    let err = app::install(&s, &slack, app_tok(APP), bot_tok(BOT_FEW_SCOPES), NOW)
        .await
        .unwrap_err();
    let SlackAppError::MissingScopes { team_id, missing } = &err else {
        panic!("expected MissingScopes, got {err:?}");
    };
    assert_eq!(team_id, TEAM);
    assert!(missing.contains(&"app_mentions:read".to_string()));
    assert!(missing.contains(&"im:history".to_string()));
    assert!(!missing.contains(&"chat:write".to_string()));
    let text = format!("{err} {}", err.recovery());
    assert!(text.contains("im:history"), "{text}");
    assert!(err.recovery().contains("reinstall"), "{}", err.recovery());
    assert_no_tokens(&text);
    assert!(mem.payloads().is_empty(), "nothing may be stored");
}

#[tokio::test]
async fn install_with_unknown_scopes_is_allowed_but_flagged() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    slack
        .bots
        .lock()
        .unwrap()
        .insert(BOT.into(), Ok(who(TEAM, None)));
    let out = app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    assert_eq!(out.summary.scopes, None);
    assert_eq!(out.summary.missing_scopes, None);
}

#[test]
fn wrong_token_types_are_rejected_by_prefix_with_a_clear_message() {
    let err = parse_app_token(BOT).unwrap_err();
    assert!(matches!(
        err,
        SlackAppError::WrongTokenType {
            expected: TokenKind::AppLevel,
            found: TokenKind::Bot
        }
    ));
    let msg = format!("{err}");
    assert!(
        msg.contains("bot token") && msg.contains("app-level"),
        "{msg}"
    );
    assert!(
        err.recovery().contains("App-Level Tokens"),
        "{}",
        err.recovery()
    );
    assert_no_tokens(&msg);

    let err = parse_bot_token(APP).unwrap_err();
    assert!(matches!(
        err,
        SlackAppError::WrongTokenType {
            expected: TokenKind::Bot,
            found: TokenKind::AppLevel
        }
    ));
    assert!(matches!(
        parse_bot_token("xoxp-test-000").unwrap_err(),
        SlackAppError::WrongTokenType {
            found: TokenKind::User,
            ..
        }
    ));
    assert!(matches!(
        parse_bot_token("not-a-token").unwrap_err(),
        SlackAppError::WrongTokenType {
            found: TokenKind::Unknown,
            ..
        }
    ));
    assert!(matches!(
        parse_bot_token("   ").unwrap_err(),
        SlackAppError::EmptyToken { .. }
    ));
    // Surrounding whitespace (a trailing newline from a file) is trimmed.
    assert_eq!(
        parse_bot_token(&format!("  {BOT}\n"))
            .unwrap()
            .expose_secret(),
        BOT
    );
    // Whitespace inside a token is never a valid token.
    assert!(parse_bot_token("xoxb-test 000").is_err());
}

#[tokio::test]
async fn revoked_bot_or_app_token_fails_install_with_recovery_and_stores_nothing() {
    let (mem, s) = store();
    let slack = FakeSlack::standard();
    let err = app::install(&s, &slack, app_tok(APP), bot_tok(BOT_REVOKED), NOW)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SlackAppError::InvalidToken { which: TokenKind::Bot, slack_error } if slack_error == "invalid_auth"),
        "{err:?}"
    );
    assert!(
        err.recovery().contains("OAuth & Permissions"),
        "{}",
        err.recovery()
    );

    let err = app::install(&s, &slack, app_tok(APP_REVOKED), bot_tok(BOT), NOW)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            SlackAppError::InvalidToken {
                which: TokenKind::AppLevel,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(
        err.recovery().contains("connections:write"),
        "{}",
        err.recovery()
    );
    assert!(mem.payloads().is_empty());
}

#[tokio::test]
async fn tokens_from_different_apps_are_rejected() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    let mut w = who(TEAM, Some(all_scopes()));
    w.app_id = Some("A00000009".into());
    slack.bots.lock().unwrap().insert(BOT.into(), Ok(w));
    let err = app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap_err();
    assert!(matches!(err, SlackAppError::AppMismatch { .. }), "{err:?}");
    assert!(s.teams().unwrap().is_empty());
}

#[tokio::test]
async fn reinstall_over_existing_state_replaces_tokens_and_keeps_install_time() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    let out = app::install(&s, &slack, app_tok(APP2), bot_tok(BOT2), NOW + 60)
        .await
        .unwrap();
    assert!(out.replaced);
    assert_eq!(out.summary.installed_at, NOW);
    assert_eq!(out.summary.verified_at, Some(NOW + 60));
    let loaded = s.load(TEAM).unwrap().unwrap();
    assert_eq!(loaded.app_token.expose_secret(), APP2);
    assert_eq!(loaded.bot_token.expose_secret(), BOT2);
    assert_eq!(
        s.teams().unwrap(),
        vec![TEAM.to_string()],
        "index has no duplicates"
    );
}

#[tokio::test]
async fn rotate_bot_token_only_keeps_the_app_token() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    let sum = app::rotate(&s, &slack, TEAM, None, Some(bot_tok(BOT2)), NOW + 5)
        .await
        .unwrap();
    assert_eq!(sum.rotated_at, Some(NOW + 5));
    let loaded = s.load(TEAM).unwrap().unwrap();
    assert_eq!(loaded.app_token.expose_secret(), APP);
    assert_eq!(loaded.bot_token.expose_secret(), BOT2);

    // App-level only.
    app::rotate(&s, &slack, TEAM, Some(app_tok(APP2)), None, NOW + 9)
        .await
        .unwrap();
    let loaded = s.load(TEAM).unwrap().unwrap();
    assert_eq!(loaded.app_token.expose_secret(), APP2);
    assert_eq!(loaded.bot_token.expose_secret(), BOT2);
}

#[tokio::test]
async fn rotate_to_a_token_from_another_workspace_is_refused_and_keeps_old_credentials() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    let err = app::rotate(
        &s,
        &slack,
        TEAM,
        None,
        Some(bot_tok(BOT_OTHER_TEAM)),
        NOW + 1,
    )
    .await
    .unwrap_err();
    let SlackAppError::WrongWorkspace { expected, actual } = &err else {
        panic!("expected WrongWorkspace, got {err:?}");
    };
    assert_eq!((expected.as_str(), actual.as_str()), (TEAM, "T00000002"));
    assert!(err.recovery().contains("install"), "{}", err.recovery());
    let loaded = s.load(TEAM).unwrap().unwrap();
    assert_eq!(loaded.bot_token.expose_secret(), BOT);
    assert!(s.load("T00000002").unwrap().is_none());
}

#[tokio::test]
async fn rotate_with_a_revoked_token_keeps_old_credentials() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    assert!(
        app::rotate(&s, &slack, TEAM, None, Some(bot_tok(BOT_REVOKED)), NOW + 1)
            .await
            .is_err()
    );
    assert_eq!(
        s.load(TEAM).unwrap().unwrap().bot_token.expose_secret(),
        BOT
    );
}

#[tokio::test]
async fn rotate_and_verify_on_a_missing_install_say_how_to_install() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    let err = app::rotate(&s, &slack, TEAM, None, Some(bot_tok(BOT)), NOW)
        .await
        .unwrap_err();
    assert!(matches!(err, SlackAppError::NotInstalled { .. }), "{err:?}");
    assert!(err.recovery().contains("slack app install"));
    let err = app::verify(&s, &slack, TEAM, NOW).await.unwrap_err();
    assert!(matches!(err, SlackAppError::NotInstalled { .. }), "{err:?}");
}

#[tokio::test]
async fn verify_reports_a_revoked_stored_token_and_updates_verified_at_when_healthy() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    let sum = app::verify(&s, &slack, TEAM, NOW + 100).await.unwrap();
    assert_eq!(sum.verified_at, Some(NOW + 100));
    assert_eq!(s.load(TEAM).unwrap().unwrap().verified_at, Some(NOW + 100));

    slack
        .bots
        .lock()
        .unwrap()
        .insert(BOT.into(), Err("token_revoked".into()));
    let err = app::verify(&s, &slack, TEAM, NOW + 200).await.unwrap_err();
    assert!(
        matches!(&err, SlackAppError::InvalidToken { which: TokenKind::Bot, slack_error } if slack_error == "token_revoked"),
        "{err:?}"
    );
    assert!(
        err.recovery().contains("slack app rotate"),
        "{}",
        err.recovery()
    );
    // A failed verify never erases credentials.
    assert!(s.load(TEAM).unwrap().is_some());
}

#[tokio::test]
async fn remove_deletes_credentials_and_index_and_is_idempotent() {
    let (mem, s) = store();
    let slack = FakeSlack::standard();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    assert!(s.remove(TEAM).unwrap());
    assert!(!s.remove(TEAM).unwrap());
    assert!(s.load(TEAM).unwrap().is_none());
    assert!(s.teams().unwrap().is_empty());
    assert!(!mem.exists(APP_CREDENTIAL_PLATFORM, TEAM));
    for p in mem.payloads() {
        assert_no_tokens(&String::from_utf8_lossy(&p));
    }
}

#[tokio::test]
async fn resolve_team_picks_the_sole_install_or_asks() {
    let (_mem, s) = store();
    let slack = FakeSlack::standard();
    assert!(matches!(
        s.resolve_team(None).unwrap_err(),
        SlackAppError::NothingInstalled
    ));
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    assert_eq!(s.resolve_team(None).unwrap(), TEAM);
    slack
        .bots
        .lock()
        .unwrap()
        .insert(BOT2.into(), Ok(who("T00000002", Some(all_scopes()))));
    app::install(&s, &slack, app_tok(APP2), bot_tok(BOT2), NOW)
        .await
        .unwrap();
    assert!(matches!(
        s.resolve_team(None).unwrap_err(),
        SlackAppError::AmbiguousTeam { .. }
    ));
    assert_eq!(s.resolve_team(Some("T00000002")).unwrap(), "T00000002");
}

fn composio(team: &str) -> SlackAuth {
    SlackAuth {
        entity_id: "entity-test".into(),
        connection_id: "conn-test".into(),
        team_id: team.into(),
        team_name: "Example Test".into(),
        user_id: "U00000002".into(),
        composio_api_key: "ck-test-0".into(),
    }
}

#[tokio::test]
async fn removing_the_app_leaves_the_composio_connection_loadable() {
    let (mem, s) = store();
    let slack = FakeSlack::standard();
    composio(TEAM).save_to(&mem).unwrap();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    s.remove(TEAM).unwrap();
    let back = SlackAuth::load_for_team_from(&mem, TEAM).unwrap();
    assert_eq!(back.connection_id, "conn-test");
}

#[tokio::test]
async fn removing_the_composio_connection_leaves_the_app_loadable() {
    let (mem, s) = store();
    let slack = FakeSlack::standard();
    composio(TEAM).save_to(&mem).unwrap();
    app::install(&s, &slack, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    SlackAuth::delete_from(&mem, TEAM).unwrap();
    assert!(SlackAuth::load_for_team_from(&mem, TEAM).is_err());
    let loaded = s.load(TEAM).unwrap().unwrap();
    assert_eq!(loaded.bot_token.expose_secret(), BOT);
    assert_eq!(s.teams().unwrap(), vec![TEAM.to_string()]);
}

#[tokio::test]
async fn corrupt_slot_is_reported_with_recovery_not_a_panic() {
    let (mem, s) = store();
    mem.put(APP_CREDENTIAL_PLATFORM, TEAM, b"{not json")
        .unwrap();
    let err = s.load(TEAM).unwrap_err();
    assert!(matches!(err, SlackAppError::Corrupt { .. }), "{err:?}");
    assert!(
        err.recovery().contains("slack app remove"),
        "{}",
        err.recovery()
    );
}

#[test]
fn api_base_override_allows_https_or_loopback_http_only() {
    use augmentagent_channel_slack::app::api_base_from;
    assert_eq!(api_base_from(None).unwrap(), "https://slack.com/api");
    assert_eq!(api_base_from(Some("  ")).unwrap(), "https://slack.com/api");
    assert_eq!(
        api_base_from(Some("http://127.0.0.1:4567/api/")).unwrap(),
        "http://127.0.0.1:4567/api"
    );
    assert!(api_base_from(Some("http://localhost:1")).is_ok());
    assert!(api_base_from(Some("http://[::1]:1")).is_ok());
    assert!(api_base_from(Some("https://slack.example.test/api")).is_ok());
    for bad in [
        "http://slack.example.test/api",
        "ftp://127.0.0.1/",
        "not a url",
        "https://user:pw@127.0.0.1/",
        "https://127.0.0.1/?token=x",
    ] {
        let err = api_base_from(Some(bad)).unwrap_err();
        assert!(
            matches!(err, SlackAppError::InvalidApiBase { .. }),
            "{bad}: {err:?}"
        );
    }
}

#[tokio::test]
async fn http_connector_installs_against_a_local_mock_and_never_keeps_the_ticket() {
    use augmentagent_channel_slack::app::HttpSlackAppConnector;
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/auth.test")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_header("x-oauth-scopes", &REQUIRED_BOT_SCOPES.join(","))
        .with_body(
            serde_json::json!({"ok": true, "team_id": TEAM, "team": "Example Test",
                "user_id": "U00000001", "bot_id": "B00000001"})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("POST", "/apps.connections.open")
        .match_header("authorization", format!("Bearer {APP}").as_str())
        .with_body(
            serde_json::json!({"ok": true,
                "url": "wss://wss.example.test/link/?ticket=ticket-test-000&app_id=A00000001"})
            .to_string(),
        )
        .create_async()
        .await;
    server
        .mock("POST", "/apps.connections.open")
        .match_header("authorization", format!("Bearer {APP_REVOKED}").as_str())
        .with_body(serde_json::json!({"ok": false, "error": "invalid_auth"}).to_string())
        .create_async()
        .await;

    let (_mem, s) = store();
    let http = HttpSlackAppConnector::new(server.url());
    let out = app::install(&s, &http, app_tok(APP), bot_tok(BOT), NOW)
        .await
        .unwrap();
    assert_eq!(out.summary.app_id.as_deref(), Some("A00000001"));
    assert_eq!(out.summary.missing_scopes, Some(vec![]));
    let dump = format!("{out:?} {:?}", s.load(TEAM).unwrap());
    assert!(!dump.contains("ticket-test-000"), "{dump}");

    let err = app::install(&s, &http, app_tok(APP_REVOKED), bot_tok(BOT), NOW)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SlackAppError::InvalidToken { which: TokenKind::AppLevel, slack_error } if slack_error == "invalid_auth"),
        "{err:?}"
    );
}
