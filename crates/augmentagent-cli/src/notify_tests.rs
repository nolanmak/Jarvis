//! #1295 — the notification registry and transport-neutral routing.
//!
//! Every producer that posts to the owner is in `notify::PRODUCERS`; a scan
//! of the workspace sources proves nothing posts outside it. Routed
//! producers go through `NotifyRouter` to Discord, Slack or both per class,
//! with a real Slack outbox (temporary store) and recording Discord sinks.
//! Fake clocks only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_channel_slack::delivery::SlackOutboxDispatcher;
use augmentagent_channel_slack::notify::{NotifyPacing, SlackNotifier, NOTIFY_KEY_PREFIX};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::RecordingSlackWebApi;
use augmentagent_store::Store;

use crate::approval_routing::Routing;
use crate::notify::{
    registry_gaps, scan_producer_sites, Delivered, DiscordTarget, Notice, NotificationSink,
    NotifyClass, NotifyRouter, Outcome, ResolvedNotice, Route, RoutedAlertSink,
    RoutedAuditNotifier, Routes, SlackOwnerSink, Surface, PRODUCERS,
};

const T0: i64 = 1_700_000_000_000;
const DM: &str = "D00000001";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A Discord stand-in that records what it was given, or fails.
#[derive(Default)]
struct RecordingDiscord {
    fail: Option<String>,
    got: Mutex<Vec<(NotifyClass, String)>>,
}

impl RecordingDiscord {
    fn failing(msg: &str) -> Self {
        Self {
            fail: Some(msg.into()),
            ..Self::default()
        }
    }
    fn bodies(&self) -> Vec<String> {
        self.got
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }
}

#[async_trait]
impl NotificationSink for RecordingDiscord {
    fn surface(&self) -> Surface {
        Surface::Discord
    }
    async fn deliver(&self, n: &ResolvedNotice, _now_ms: i64) -> anyhow::Result<Delivered> {
        if let Some(e) = &self.fail {
            anyhow::bail!("{e}");
        }
        self.got.lock().unwrap().push((n.class, n.body.clone()));
        Ok(Delivered::Posted)
    }
}

/// A Slack sink that always fails (the outbox is unavailable).
struct BrokenSlack;

#[async_trait]
impl NotificationSink for BrokenSlack {
    fn surface(&self) -> Surface {
        Surface::Slack
    }
    async fn deliver(&self, _n: &ResolvedNotice, _now_ms: i64) -> anyhow::Result<Delivered> {
        anyhow::bail!("slack outbox unavailable")
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("agent.db")).unwrap());
        Self { _dir: dir, store }
    }

    fn slack(&self) -> Arc<SlackOwnerSink> {
        let dm = SlackWorkspace::new("T00000001", None)
            .unwrap()
            .conversation(DM, None)
            .unwrap();
        Arc::new(SlackOwnerSink::fixed(
            SlackNotifier::new(Arc::clone(&self.store), dm).with_pacing(NotifyPacing {
                late_after_ms: 5 * 60_000,
                spacing_ms: 0,
            }),
        ))
    }

    /// Texts queued for Slack, delivered through the real dispatcher.
    async fn slack_posts(&self) -> Vec<(String, String)> {
        let api = RecordingSlackWebApi::default();
        let ws = SlackWorkspace::new("T00000001", None).unwrap();
        SlackOutboxDispatcher::new(&self.store, &api, &ws)
            .drain(T0 + 1)
            .await
            .unwrap();
        api.messages()
            .into_iter()
            .map(|m| (m.channel, m.text))
            .collect()
    }

    fn slack_keys(&self) -> Vec<String> {
        let ws = SlackWorkspace::new("T00000001", None).unwrap();
        self.store
            .outbound_sends_with_key_prefix(&ws.account(), NOTIFY_KEY_PREFIX, &[])
            .unwrap()
            .into_iter()
            .map(|s| s.idempotency_key)
            .collect()
    }
}

fn routes(pairs: &[(&str, &str)]) -> Routes {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (routes, errors) = Routes::from_lookup(|k| map.get(k).cloned());
    assert!(errors.is_empty(), "{errors:?}");
    routes
}

fn router(f: &Fixture, discord: Option<Arc<RecordingDiscord>>, r: Routes) -> NotifyRouter {
    let mut router = NotifyRouter::new(r)
        .with_slack(f.slack() as Arc<dyn NotificationSink>)
        .with_clock(Arc::new(|| T0));
    if let Some(d) = discord {
        router = router
            .with_discord(
                DiscordTarget::Channel,
                Arc::clone(&d) as Arc<dyn NotificationSink>,
            )
            .with_discord(DiscordTarget::Webhook, d as Arc<dyn NotificationSink>);
    }
    router
}

// ---------------------------------------------------------------------------
// Registry completeness
// ---------------------------------------------------------------------------

#[test]
fn registry_covers_every_producer_in_the_code() {
    let found = scan_producer_sites(&workspace_root());
    let gaps = registry_gaps(&found, PRODUCERS);
    assert!(
        gaps.is_empty(),
        "producers posting to the owner without a registry entry (or stale entries); add them \
         to notify::PRODUCERS:\n{}",
        gaps.join("\n")
    );
}

/// #1416 — the interrupted-turn notice added in #1396 posted from
/// `event_handler.rs` without the registry totals being bumped, so both
/// completeness tests failed on main. The registry must cover every
/// `event_handler.rs` posting site, as a `Reply`.
#[test]
fn the_discord_event_handler_reply_sites_are_all_registered() {
    const FILE: &str = "crates/augmentagent-approval-discord/src/event_handler.rs";
    let found = scan_producer_sites(&workspace_root());
    for marker in ["send_message(&", "CreateMessage::new("] {
        let scanned = found
            .get(&(FILE.to_string(), marker))
            .copied()
            .unwrap_or_default();
        let registered: usize = PRODUCERS
            .iter()
            .filter(|p| p.file == FILE && p.marker == marker && p.route == Route::Reply)
            .map(|p| p.sites)
            .sum();
        assert_eq!(registered, scanned, "`{marker}` sites in {FILE}");
    }
}

#[test]
fn a_producer_missing_from_the_registry_fails_the_check() {
    let found = scan_producer_sites(&workspace_root());
    let without: Vec<_> = PRODUCERS
        .iter()
        .filter(|p| p.id != "research_digest")
        .cloned()
        .collect();
    let gaps = registry_gaps(&found, &without);
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert!(
        gaps[0].contains("crates/augmentagent-cli/src/research.rs")
            && gaps[0].contains("notify_owner("),
        "{gaps:?}"
    );
}

#[test]
fn a_new_discord_post_outside_the_registry_fails_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("crates/augmentagent-newthing/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "async fn nag(h: &Http, c: ChannelId) {\n    c.send_message(&h, CreateMessage::new().content(\"hi\")).await;\n}\n\
         #[cfg(test)]\nmod tests {\n    fn t() { CreateMessage::new(); }\n}\n",
    )
    .unwrap();
    let found = scan_producer_sites(dir.path());
    let gaps = registry_gaps(&found, &[]);
    assert_eq!(gaps.len(), 2, "one per marker, test code ignored: {gaps:?}");
    assert!(gaps
        .iter()
        .all(|g| g.contains("crates/augmentagent-newthing/src/lib.rs")));
}

#[test]
fn every_routed_class_defaults_to_every_configured_surface() {
    let r = routes(&[]);
    for class in NotifyClass::ALL {
        assert_eq!(r.for_class(class), Routing::AUTO, "{class:?}");
    }
    let classes: Vec<NotifyClass> = PRODUCERS
        .iter()
        .filter_map(|p| match p.route {
            Route::Routed { class, .. } => Some(class),
            _ => None,
        })
        .collect();
    for class in NotifyClass::ALL {
        assert!(classes.contains(&class), "{class:?} has no producer");
    }
}

#[test]
fn actionable_producers_are_never_fanned_out_blindly() {
    // The owner acts on approval cards (kept in step across surfaces by
    // CardSurfaces, #1289) and loop reminders (posted only to the loop's
    // own surface, #1292). Nothing routed here asks for an action, so a
    // fan-out never makes the owner act twice.
    for p in PRODUCERS {
        if p.actionable {
            assert!(
                matches!(p.route, Route::ApprovalSurfaces | Route::LoopDestination),
                "{} ({}) is actionable but routed {:?}",
                p.id,
                p.what,
                p.route
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Each routed producer reaches Slack with its content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_routed_producer_reaches_slack_with_its_content() {
    let f = Fixture::new();
    let router = Arc::new(router(
        &f,
        None,
        routes(&[("AUGMENTAGENT_NOTIFY_SURFACES", "slack")]),
    ));
    let mut expected = Vec::new();
    for p in PRODUCERS {
        let Route::Routed { class, .. } = p.route else {
            continue;
        };
        let body = format!("{} says hello", p.id);
        let out = match p.id {
            // Adapters the calendar channel and the reasoner call.
            "calendar_alert" => {
                use augmentagent_channel_calendar::AlertSink;
                RoutedAlertSink::new(Arc::clone(&router))
                    .send(&body)
                    .await
                    .unwrap();
                None
            }
            "tool_audit" => {
                use augmentagent_channel_core::AuditNotifier;
                let record = augmentagent_channel_core::build_audit_record(
                    augmentagent_channel_core::ProviderKind::Claude,
                    "2026-09-29T07:00:00Z".into(),
                    "s-1".into(),
                    "Bash".into(),
                    serde_json::json!({ "command": body.clone() }),
                    "",
                    false,
                );
                RoutedAuditNotifier::new(Arc::clone(&router), None)
                    .notify("s-1", &record)
                    .await;
                None
            }
            _ => Some(router.deliver(Notice::new(p.id, body.clone()), None).await),
        };
        if let Some(out) = out {
            assert_eq!(out.class, class);
            assert!(
                matches!(
                    out.outcome(Surface::Slack),
                    Outcome::Delivered(Delivered::Queued)
                ),
                "{}: {out:?}",
                p.id
            );
        }
        expected.push((p.id, class, body));
    }
    let posts = f.slack_posts().await;
    assert_eq!(posts.len(), expected.len(), "{posts:?}");
    for (id, _class, body) in &expected {
        assert!(
            posts
                .iter()
                .any(|(ch, text)| ch == DM && text.contains(body.as_str())),
            "{id} did not reach the owner DM: {posts:?}"
        );
    }
    let keys = f.slack_keys();
    for (_, class, _) in &expected {
        assert!(
            keys.iter()
                .any(|k| k.starts_with(&format!("turn:notify:{}:", class.as_str()))),
            "{class:?}: {keys:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Fan-out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slack_only_needs_no_discord_credentials() {
    let f = Fixture::new();
    // No Discord sink at all, routing left at its default.
    let router = router(&f, None, routes(&[]));
    let out = router
        .deliver(Notice::new("email_digest", "digest body"), None)
        .await;
    assert!(matches!(
        out.outcome(Surface::Slack),
        Outcome::Delivered(Delivered::Queued)
    ));
    assert!(matches!(
        out.outcome(Surface::Discord),
        Outcome::NotConfigured
    ));
    assert!(out.into_result().is_ok());
    assert_eq!(
        f.slack_posts().await,
        vec![(DM.into(), "digest body".into())]
    );
}

#[tokio::test]
async fn all_surfaces_get_it_when_all_are_enabled() {
    let f = Fixture::new();
    let discord = Arc::new(RecordingDiscord::default());
    let router = router(&f, Some(Arc::clone(&discord)), routes(&[]));
    let out = router
        .deliver(Notice::new("research_digest", "papers"), None)
        .await;
    assert!(matches!(
        out.outcome(Surface::Discord),
        Outcome::Delivered(Delivered::Posted)
    ));
    assert!(matches!(
        out.outcome(Surface::Slack),
        Outcome::Delivered(Delivered::Queued)
    ));
    assert_eq!(discord.bodies(), vec!["papers".to_string()]);
    assert_eq!(f.slack_posts().await.len(), 1);
}

#[tokio::test]
async fn a_failing_discord_does_not_block_slack_and_is_reported() {
    let f = Fixture::new();
    let discord = Arc::new(RecordingDiscord::failing("discord 503"));
    let router = router(&f, Some(discord), routes(&[]));
    let out = router
        .deliver(Notice::new("autopr_health", "2 alerts"), None)
        .await;
    assert!(matches!(
        out.outcome(Surface::Slack),
        Outcome::Delivered(Delivered::Queued)
    ));
    match out.outcome(Surface::Discord) {
        Outcome::Failed(e) => assert!(e.contains("discord 503"), "{e}"),
        other => panic!("expected a visible failure, got {other:?}"),
    }
    assert_eq!(out.failures().len(), 1);
    assert!(out.into_result().is_ok(), "one surface got it");
    assert_eq!(f.slack_posts().await.len(), 1);
}

#[tokio::test]
async fn a_failing_slack_does_not_block_discord() {
    let discord = Arc::new(RecordingDiscord::default());
    let router = NotifyRouter::new(routes(&[]))
        .with_slack(Arc::new(BrokenSlack) as Arc<dyn NotificationSink>)
        .with_discord(
            DiscordTarget::Webhook,
            Arc::clone(&discord) as Arc<dyn NotificationSink>,
        )
        .with_clock(Arc::new(|| T0));
    let out = router
        .deliver(Notice::new("self_improve", "draft PR #9"), None)
        .await;
    assert!(matches!(
        out.outcome(Surface::Discord),
        Outcome::Delivered(Delivered::Posted)
    ));
    assert!(matches!(out.outcome(Surface::Slack), Outcome::Failed(_)));
    assert_eq!(discord.bodies(), vec!["draft PR #9".to_string()]);
}

#[tokio::test]
async fn nothing_delivered_anywhere_is_an_error() {
    let router = NotifyRouter::new(routes(&[]))
        .with_slack(Arc::new(BrokenSlack) as Arc<dyn NotificationSink>)
        .with_clock(Arc::new(|| T0));
    let err = router
        .deliver(Notice::new("email_digest", "x"), None)
        .await
        .into_result()
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("slack outbox unavailable"),
        "{err:#}"
    );
    let none = NotifyRouter::new(routes(&[])).with_clock(Arc::new(|| T0));
    assert!(none
        .deliver(Notice::new("email_digest", "x"), None)
        .await
        .into_result()
        .is_err());
}

#[tokio::test]
async fn a_retry_after_a_partial_fan_out_does_not_post_twice_on_slack() {
    let f = Fixture::new();
    let failing = Arc::new(RecordingDiscord::failing("discord down"));
    let first = router(&f, Some(failing), routes(&[]))
        .deliver(Notice::new("email_digest", "same digest"), None)
        .await;
    assert!(matches!(
        first.outcome(Surface::Slack),
        Outcome::Delivered(Delivered::Queued)
    ));
    // The producer runs again once Discord is back.
    let discord = Arc::new(RecordingDiscord::default());
    let second = router(&f, Some(Arc::clone(&discord)), routes(&[]))
        .deliver(Notice::new("email_digest", "same digest"), None)
        .await;
    assert!(matches!(
        second.outcome(Surface::Slack),
        Outcome::Delivered(Delivered::Duplicate)
    ));
    assert_eq!(discord.bodies(), vec!["same digest".to_string()]);
    assert_eq!(f.slack_posts().await.len(), 1, "Slack got it once");
}

// ---------------------------------------------------------------------------
// Per-class routing
// ---------------------------------------------------------------------------

#[test]
fn routing_is_configurable_per_class() {
    let r = routes(&[
        ("AUGMENTAGENT_NOTIFY_SURFACES", "discord"),
        ("AUGMENTAGENT_NOTIFY_SURFACES_AUDIT", "slack"),
        ("AUGMENTAGENT_NOTIFY_SURFACES_HEALTH", "discord,slack"),
    ]);
    let discord_only = Routing {
        discord: true,
        slack: false,
    };
    let slack_only = Routing {
        discord: false,
        slack: true,
    };
    assert_eq!(r.for_class(NotifyClass::Digest), discord_only);
    assert_eq!(r.for_class(NotifyClass::Audit), slack_only);
    assert_eq!(r.for_class(NotifyClass::Health), Routing::AUTO);
    assert_eq!(
        NotifyClass::Reminder.env_var(),
        "AUGMENTAGENT_NOTIFY_SURFACES_REMINDER"
    );

    let map: BTreeMap<&str, &str> = [
        ("AUGMENTAGENT_NOTIFY_SURFACES", "teams"),
        ("AUGMENTAGENT_NOTIFY_SURFACES_DIGEST", "slack"),
    ]
    .into_iter()
    .collect();
    let (r, errors) = Routes::from_lookup(|k| map.get(k).map(|v| v.to_string()));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("teams"));
    assert_eq!(
        r.for_class(NotifyClass::Review),
        Routing::AUTO,
        "bad default falls back to auto"
    );
    assert_eq!(r.for_class(NotifyClass::Digest), slack_only);
}

#[tokio::test]
async fn a_class_routed_to_one_surface_skips_the_other() {
    let f = Fixture::new();
    let discord = Arc::new(RecordingDiscord::default());
    let router = router(
        &f,
        Some(Arc::clone(&discord)),
        routes(&[
            ("AUGMENTAGENT_NOTIFY_SURFACES_AUDIT", "slack"),
            ("AUGMENTAGENT_NOTIFY_SURFACES_DIGEST", "discord"),
        ]),
    );
    let audit = router
        .deliver(Notice::new("tool_audit", "Bash ran"), None)
        .await;
    assert!(matches!(
        audit.outcome(Surface::Discord),
        Outcome::NotRouted
    ));
    let digest = router
        .deliver(Notice::new("email_digest", "digest"), None)
        .await;
    assert!(matches!(digest.outcome(Surface::Slack), Outcome::NotRouted));
    assert_eq!(discord.bodies(), vec!["digest".to_string()]);
    let posts = f.slack_posts().await;
    assert_eq!(posts, vec![(DM.into(), "Bash ran".into())]);
}

#[tokio::test]
async fn audit_notices_from_a_discord_request_go_to_that_channel_and_slack() {
    use augmentagent_channel_core::AuditNotifier;
    let f = Fixture::new();
    let origin = Arc::new(RecordingDiscord::default());
    let bot_channel = Arc::new(RecordingDiscord::default());
    let router = Arc::new(router(&f, Some(Arc::clone(&bot_channel)), routes(&[])));
    let record = augmentagent_channel_core::build_audit_record(
        augmentagent_channel_core::ProviderKind::Claude,
        "2026-09-29T07:00:00Z".into(),
        "c-1:m-1".into(),
        "Bash".into(),
        serde_json::json!({ "command": "rm -rf build" }),
        "",
        false,
    );
    let notifier = RoutedAuditNotifier::new(
        Arc::clone(&router),
        Some(Arc::clone(&origin) as Arc<dyn NotificationSink>),
    );
    notifier.notify("c-1:m-1", &record).await;
    // The same record reported twice (a replayed stream) posts once on Slack.
    notifier.notify("c-1:m-1", &record).await;
    let notice = augmentagent_channel_core::format_notice(&record);
    assert_eq!(origin.bodies(), vec![notice.clone(), notice.clone()]);
    assert!(bot_channel.bodies().is_empty(), "not the shared channel");
    let posts = f.slack_posts().await;
    assert_eq!(posts.len(), 1);
    assert!(posts[0].1.contains("rm -rf build"), "{posts:?}");
}

#[test]
fn unknown_producers_are_rejected() {
    assert!(crate::notify::producer("email_digest").is_some());
    assert!(crate::notify::producer("no_such_producer").is_none());
}
