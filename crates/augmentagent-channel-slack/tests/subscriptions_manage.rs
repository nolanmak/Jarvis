//! #1296 — managing Slack subscriptions by name or ID: channels, group DMs
//! and DMs (people resolved through the wiki identity layer), mode changes,
//! unsubscribe, and clear failures for unknown or ambiguous targets. The
//! same API backs the CLI and the Slack command hook (#1292).

use std::path::Path;

use augmentagent_channel_slack::subscriptions::{
    run_command, ConversationKind, DirectoryEntry, StaticDirectory, SubscriptionChange,
    SubscriptionError, SubscriptionManager,
};
use augmentagent_store::{Store, SubscriptionMode};

const TEAM: &str = "T0000001";

fn entry(id: &str, name: &str, kind: ConversationKind, user: Option<&str>) -> DirectoryEntry {
    DirectoryEntry {
        id: id.into(),
        name: name.into(),
        kind,
        user: user.map(str::to_string),
    }
}

fn directory() -> StaticDirectory {
    use ConversationKind::*;
    StaticDirectory(vec![
        entry("C0000001", "general", Channel, None),
        entry("C0000002", "launch", Channel, None),
        entry("G0000003", "launch-private", PrivateChannel, None),
        entry("C0000004", "design", Channel, None),
        entry("C0000005", "design", Channel, None),
        entry("C0000006", "mpdm-alice--bob--owner-1", GroupDm, None),
        entry("D0000007", "", Dm, Some("U000000B")),
        entry("D0000008", "", Dm, Some("U000000C")),
    ])
}

fn page(wiki: &Path, slug: &str, title: &str, slack: Option<&str>) {
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    let identities = slack
        .map(|id| format!("  slack: {id}\n"))
        .unwrap_or_else(|| "  email: someone@example.com\n".into());
    std::fs::write(
        wiki.join("people").join(format!("{slug}.md")),
        format!("---\nkind: person\nidentities:\n{identities}---\n# {title}\n"),
    )
    .unwrap();
}

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    wiki: std::path::PathBuf,
    dir: StaticDirectory,
}

fn world() -> World {
    let d = tempfile::tempdir().unwrap();
    let store = Store::open(d.path().join("data.db")).unwrap();
    let wiki = d.path().join("wiki ü");
    page(&wiki, "alice-example", "Alice Example", Some("U000000B"));
    page(&wiki, "alex-one", "Alex One", Some("U000000C"));
    page(&wiki, "alex-two", "Alex Two", Some("U000000D"));
    page(&wiki, "carol-nobody", "Carol Nobody", None);
    World {
        _dir: d,
        store,
        wiki,
        dir: directory(),
    }
}

impl World {
    fn manager(&self) -> SubscriptionManager<'_> {
        SubscriptionManager::new(&self.store, TEAM)
            .with_wiki_root(Some(self.wiki.clone()))
            .with_directory(&self.dir)
    }

    fn active(&self) -> Vec<(String, String, String)> {
        self.store
            .list_active_subscriptions("slack")
            .unwrap()
            .into_iter()
            .map(|s| (s.channel_id, s.display_name, s.mode.as_str().to_string()))
            .collect()
    }
}

#[tokio::test]
async fn subscribe_a_channel_by_name_then_change_mode_then_unsubscribe() {
    let w = world();
    let m = w.manager();
    let change = m
        .subscribe("#launch", SubscriptionMode::Digest, None)
        .await
        .unwrap();
    let SubscriptionChange::Subscribed {
        subscription: sub,
        created: true,
    } = &change
    else {
        panic!("{change:?}");
    };
    assert_eq!(sub.channel_id, "C0000002");
    assert_eq!(sub.display_name, "#launch");
    assert_eq!(sub.account_id.as_deref(), Some(TEAM));
    assert!(change.describe().contains("#launch"));

    // Mode by bare name, unsubscribe by ID; the row keeps its ID.
    let changed = m
        .set_mode("launch", SubscriptionMode::Priority)
        .await
        .unwrap();
    assert!(matches!(
        changed,
        SubscriptionChange::ModeChanged {
            from: SubscriptionMode::Digest,
            ..
        }
    ));
    assert_eq!(changed.subscription().id, sub.id);
    assert_eq!(
        w.active(),
        vec![("C0000002".into(), "#launch".into(), "priority".into())]
    );
    let gone = m.unsubscribe("C0000002").await.unwrap();
    assert_eq!(gone.subscription().id, sub.id);
    assert!(w.active().is_empty());
    // Re-subscribing reuses the same row (ID and cursor kept).
    w.store
        .update_last_seen_message(&sub.id, "1800000000.000100")
        .unwrap();
    let again = m
        .subscribe("C0000002", SubscriptionMode::StoreOnly, None)
        .await
        .unwrap();
    assert!(matches!(
        again,
        SubscriptionChange::Subscribed { created: false, .. }
    ));
    assert_eq!(again.subscription().id, sub.id);
    assert_eq!(
        again.subscription().last_seen_message_id.as_deref(),
        Some("1800000000.000100")
    );
}

#[tokio::test]
async fn subscribe_a_dm_by_person_and_a_group_dm_by_name() {
    let w = world();
    let m = w.manager();
    let dm = m
        .subscribe("Alice", SubscriptionMode::Priority, None)
        .await
        .unwrap();
    assert_eq!(dm.subscription().channel_id, "D0000007");
    assert_eq!(dm.subscription().display_name, "DM with Alice Example");
    // By Slack mention and by user ID too.
    assert_eq!(
        m.resolve("<@U000000B>").await.unwrap().channel_id,
        "D0000007"
    );
    let group = m
        .subscribe("mpdm-alice--bob--owner-1", SubscriptionMode::Digest, None)
        .await
        .unwrap();
    assert_eq!(group.subscription().channel_id, "C0000006");
    assert_eq!(
        group.subscription().display_name,
        "group DM with alice, bob, owner"
    );
    // A private channel by its `<#G…|name>` mention.
    let private = m.resolve("<#G0000003|launch-private>").await.unwrap();
    assert_eq!(private.channel_id, "G0000003");
    assert_eq!(private.kind, Some(ConversationKind::PrivateChannel));
    // Mode change and unsubscribe of the DM by person name.
    m.set_mode("Alice Example", SubscriptionMode::Digest)
        .await
        .unwrap();
    m.unsubscribe("alice-example").await.unwrap();
    assert_eq!(
        w.active(),
        vec![(
            "C0000006".into(),
            "group DM with alice, bob, owner".into(),
            "digest".into()
        )]
    );
}

#[tokio::test]
async fn ambiguous_targets_ask_and_change_nothing() {
    let w = world();
    let m = w.manager();
    // Two channels named #design.
    let err = m
        .subscribe("#design", SubscriptionMode::Digest, None)
        .await
        .unwrap_err();
    let SubscriptionError::Ambiguous { candidates, .. } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().any(|c| c.contains("C0000004")));
    assert!(candidates.iter().any(|c| c.contains("C0000005")));
    assert!(err.to_string().starts_with("Which one?"));
    // Two people called Alex.
    let err = m
        .subscribe("Alex", SubscriptionMode::Digest, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SubscriptionError::Ambiguous { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("Alex One"));
    assert!(w.active().is_empty(), "nothing subscribed");
    // The ID settles it.
    m.subscribe("C0000005", SubscriptionMode::Digest, None)
        .await
        .unwrap();
    assert_eq!(w.active().len(), 1);
}

#[tokio::test]
async fn unknown_targets_fail_clearly() {
    let w = world();
    let m = w.manager();
    for (q, needle) in [
        ("#no-such-channel", "no-such-channel"),
        ("Zed Nobody", "Zed Nobody"),
        // In the wiki but without a Slack identity.
        ("Carol Nobody", "no Slack identity"),
        // A Slack person with no DM open with the owner.
        ("Alex Two", "no DM"),
    ] {
        let err = m
            .subscribe(q, SubscriptionMode::Digest, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SubscriptionError::Unknown { .. }),
            "{q}: {err:?}"
        );
        assert!(err.to_string().contains(needle), "{q}: {err}");
    }
    let err = m
        .set_mode("#general", SubscriptionMode::Digest)
        .await
        .unwrap_err();
    assert!(
        matches!(err, SubscriptionError::NotSubscribed { .. }),
        "{err:?}"
    );
    let err = m.unsubscribe("#no-such-channel").await.unwrap_err();
    assert!(!matches!(err, SubscriptionError::Store(_)), "{err:?}");
    assert!(w.active().is_empty());
}

#[tokio::test]
async fn without_a_directory_ids_still_work_and_names_fail_clearly() {
    let w = world();
    let m = SubscriptionManager::new(&w.store, TEAM);
    m.subscribe("C0000009", SubscriptionMode::Digest, Some("#ops"))
        .await
        .unwrap();
    assert_eq!(
        w.active(),
        vec![("C0000009".into(), "#ops".into(), "digest".into())]
    );
    // Existing subscriptions resolve by name offline.
    m.set_mode("#ops", SubscriptionMode::Priority)
        .await
        .unwrap();
    let err = m
        .subscribe("#launch", SubscriptionMode::Digest, None)
        .await
        .unwrap_err();
    assert!(matches!(err, SubscriptionError::Unknown { .. }), "{err:?}");
}

#[tokio::test]
async fn subscriptions_are_scoped_to_their_workspace() {
    let w = world();
    w.store
        .upsert_subscription(
            "slack",
            "C0000001",
            "#general",
            SubscriptionMode::Digest,
            Some("T0000099"),
        )
        .unwrap();
    let m = w.manager();
    assert!(m.list().unwrap().is_empty(), "another workspace's row");
    let err = m.unsubscribe("#general").await.unwrap_err();
    assert!(
        matches!(err, SubscriptionError::NotSubscribed { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_slack_command_hook_speaks_text() {
    let w = world();
    let m = w.manager();
    let out = run_command(&m, "subscribe #launch priority").await;
    assert!(out.contains("#launch") && out.contains("priority"), "{out}");
    let out = run_command(&m, "list").await;
    assert!(out.contains("#launch") && out.contains("C0000002"), "{out}");
    let out = run_command(&m, "mode #launch digest").await;
    assert!(out.contains("digest"), "{out}");
    let out = run_command(&m, "subscribe #design").await;
    assert!(out.starts_with("Which one?"), "{out}");
    let out = run_command(&m, "subscribe #launch loud").await;
    assert!(out.contains("priority, digest or store_only"), "{out}");
    let out = run_command(&m, "unsubscribe #launch").await;
    assert!(out.contains("Unsubscribed"), "{out}");
    let out = run_command(&m, "").await;
    assert!(
        out.contains("No Slack conversations are subscribed"),
        "{out}"
    );
    let out = run_command(&m, "frobnicate").await;
    assert!(out.contains("subscribe <"), "{out}");
}

/// #1296 × #1292 — the same operations as owner commands on Slack
/// (`/jarvis subscribe …`, `!unsubscribe …`, `subscriptions`), through the
/// shared command registry.
#[tokio::test]
async fn subscription_owner_commands_run_through_the_registry() {
    use augmentagent_channel_slack::commands::{
        recognize, CommandContext, ConversationControl, SlackCommandDeps, SlackCommands,
    };
    use augmentagent_channel_slack::surface::SlackWorkspace;
    use augmentagent_store::SurfaceConversationRef;
    use std::sync::Arc;

    struct Idle;
    impl ConversationControl for Idle {
        fn is_running(&self, _: &SurfaceConversationRef) -> bool {
            false
        }
        fn cancel_running(&self, _: &SurfaceConversationRef) -> bool {
            false
        }
    }
    let w = world();
    let store = Arc::new(Store::open(w.store.db_path()).unwrap());
    let mut deps = SlackCommandDeps::new(w.wiki.join("model-selection.json"));
    deps.wiki_root = Some(w.wiki.clone());
    deps.subscription_directory = Some(Arc::new(|team: &str| {
        assert_eq!(team, TEAM);
        Some(Arc::new(directory())
            as Arc<
                dyn augmentagent_channel_slack::subscriptions::ConversationDirectory,
            >)
    }));
    let commands = SlackCommands::new(store, deps);
    let ws = SlackWorkspace::new(TEAM, None).unwrap();
    let owner = ws.owner("U000000A").unwrap();
    let dm = ws.conversation("D0000099", None).unwrap();
    let run = |text: &'static str, slash: bool| {
        let commands = &commands;
        let owner = &owner;
        let dm = &dm;
        async move {
            let r = recognize(text, slash).unwrap_or_else(|| panic!("{text} is a command"));
            commands
                .execute(
                    &r,
                    &CommandContext {
                        owner,
                        conversation: dm,
                        control: &Idle,
                        now_ms: 0,
                    },
                )
                .await
        }
    };
    let out = run("subscribe #launch priority", true).await;
    assert!(
        out.contains("Subscribed to #launch (`C0000002`) in priority mode."),
        "{out}"
    );
    let out = run("!subscribe Alice", false).await;
    assert!(out.contains("DM with Alice Example"), "{out}");
    let out = run("subscriptions", false).await;
    assert!(out.contains("#launch") && out.contains("D0000007"), "{out}");
    let out = run("subscriptions mode #launch digest", true).await;
    assert!(out.contains("is now digest (was priority)"), "{out}");
    let out = run("!subscribe #design", false).await;
    assert!(out.starts_with("Which one?"), "{out}");
    let out = run("unsubscribe #launch", true).await;
    assert!(out.contains("Unsubscribed from #launch"), "{out}");
    assert_eq!(w.active().len(), 1);
    // A sentence that starts with the word is a question for the agent.
    assert!(recognize("subscribe me to the newsletter please", false).is_none());
}
