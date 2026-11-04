//! #1833 C10 (D16) — heartbeat proactive outreach exit tests. Synthetic data
//! only: a temp store, a recording approval broker, a fake executor that
//! queues the iMessage outbox like `approve_imessage`, and a fake composer
//! that stores the same synthetic compose row `imessage compose` does.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_autonomy::broker::AutonomyBroker;
use augmentagent_autonomy::gate::{AutonomyGate, GateDeps, Reason};
use augmentagent_autonomy::outreach::{
    Composed, OutreachComposer, OutreachDecision, OutreachEngine, OutreachReport,
    PROACTIVE_THREAD_PREFIX,
};
use augmentagent_autonomy::{
    normalize_handle, Handle, PlanStatus, Tier, TrustSnapshot, TrustedContact,
};
use augmentagent_channel_core::reasoner::{Reasoner, ReasonerOpts};
use augmentagent_heartbeat::{HeartbeatConfig, HeartbeatRunner, Outreach, RunOptions};
use augmentagent_store::{Email, Store};
use common::*;

const ACTIONS_SLUG: &str = "bob-roe";
const ACTIONS_PHONE: &str = "+12025550142"; // pii-ok synthetic
const PENDING_SLUG: &str = "carl-poe";
const PENDING_PHONE: &str = "+12025550143"; // pii-ok synthetic

/// Stores the synthetic `kind = compose` row + pending action exactly as
/// `imessage compose` does, and records every call.
struct FakeComposer {
    store: Arc<Store>,
    checks: Mutex<Vec<(String, String)>>,
    composes: Mutex<Vec<(String, String)>>,
}

impl FakeComposer {
    fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            store,
            checks: Mutex::new(Vec::new()),
            composes: Mutex::new(Vec::new()),
        })
    }
    fn composes(&self) -> Vec<(String, String)> {
        self.composes.lock().unwrap().clone()
    }
    fn checks(&self) -> Vec<(String, String)> {
        self.checks.lock().unwrap().clone()
    }
}

#[async_trait]
impl OutreachComposer for FakeComposer {
    async fn check(&self, target: &Handle, text: &str) -> Result<String, String> {
        self.checks
            .lock()
            .unwrap()
            .push((target.canonical(), text.to_string()));
        Ok(format!("new iMessage to {}", target.value()))
    }
    async fn compose(&self, target: &Handle, text: &str) -> Result<Composed, String> {
        self.composes
            .lock()
            .unwrap()
            .push((target.canonical(), text.to_string()));
        let email = Email {
            message_id: format!("imessage-compose:{}", self.composes().len()),
            thread_id: Some(format!("imessage:{}", target.value())),
            from: format!("Jane <{}>", target.value()),
            to: target.value().to_string(),
            cc: String::new(),
            attachments: Vec::new(),
            subject: text.lines().next().unwrap_or_default().into(),
            body: text.into(),
            date: chrono::Utc::now().to_rfc3339(),
            account_entity_id: Some("imessage".into()),
            platform: "imessage".into(),
            kind: "compose".into(),
        };
        let (action_id, _) = self
            .store
            .create_imessage_compose(&email, text)
            .map_err(|e| e.to_string())?;
        Ok(Composed { action_id, email })
    }
}

fn full() -> TrustedContact {
    contact(Tier::Full, &[])
}

fn other(slug: &str, phone: &str, tier: Tier) -> TrustedContact {
    let h = normalize_handle("imessage", phone).unwrap();
    TrustedContact {
        slug: slug.into(),
        tier,
        allowed_actions: if tier == Tier::Actions {
            vec!["calendar.send_details".into()]
        } else {
            Vec::new()
        },
        handles: vec![h.clone()],
        calendar_email: None,
        preferred_handle: h,
        daily_cap: 10,
        entry_hash: format!("{slug:0<32}"),
    }
}

/// jane-doe (full, active), bob-roe (actions, active), carl-poe (full,
/// pending).
fn mixed_snapshot() -> TrustSnapshot {
    TrustSnapshot {
        active: vec![full(), other(ACTIONS_SLUG, ACTIONS_PHONE, Tier::Actions)],
        pending: vec![other(PENDING_SLUG, PENDING_PHONE, Tier::Full)],
        ..TrustSnapshot::default()
    }
}

struct Fx {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    inner: Arc<Recording>,
    exec: Arc<FakeExecutor>,
    composer: Arc<FakeComposer>,
    engine: Arc<OutreachEngine>,
    wiki: std::path::PathBuf,
}

fn fx_with(trust: TrustSnapshot, master_on: bool) -> Fx {
    let (dir, store) = store();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    std::fs::write(
        wiki.join("people").join(format!("{SLUG}.md")),
        "---\nidentities:\n  imessage: [\"+12025550100\"]\n---\n# Jane Doe\n",
    )
    .unwrap();
    let inner = Recording::new(true);
    let exec = FakeExecutor::new(Arc::clone(&store));
    let gate = Arc::new(AutonomyGate::new(GateDeps {
        store: Arc::clone(&store),
        master_on,
        trust: Arc::new(trust),
        calendar: None,
        planner: None,
        owner_addresses: Vec::new(),
        tz: chrono_tz::UTC,
    }));
    let broker = Arc::new(AutonomyBroker::new(
        Arc::clone(&inner) as Arc<dyn ApprovalBroker>,
        Arc::clone(&gate),
        exec.clone(),
        None,
    ));
    let composer = FakeComposer::new(Arc::clone(&store));
    let engine = Arc::new(OutreachEngine {
        gate,
        broker: Some(broker),
        composer: composer.clone(),
        wiki_root: Some(wiki.clone()),
        notices_deliverable: true,
    });
    Fx {
        _dir: dir,
        store,
        inner,
        exec,
        composer,
        engine,
        wiki,
    }
}

fn fixture() -> Fx {
    fx_with(snapshot(full()), true)
}

/// Start of the current 30-minute idempotency window plus one minute, so
/// tests can step across windows deliberately.
fn window_start() -> i64 {
    let w = augmentagent_autonomy::IDEM_WINDOW_MS;
    now_ms().div_euclid(w) * w + 60_000
}

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;

async fn send(fx: &Fx, text: &str, at: i64) -> OutreachReport {
    fx.engine
        .run(SLUG, text, "deck is due today", false, at)
        .await
}

fn noted(fx: &Fx) -> Vec<(String, String)> {
    fx.inner
        .cards()
        .into_iter()
        .filter_map(|c| match c {
            Call::Noted {
                action_id, note, ..
            } => Some((action_id, note)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn outreach_runs_through_gate_with_proactive_trigger() {
    let fx = fixture();
    let text = "Hey, can you send me the deck before 3?";
    let r = send(&fx, text, now_ms()).await;
    assert_eq!(r.decision, OutreachDecision::AutoSent, "{r:?}");
    assert_eq!(r.status(), "auto-sent");
    let action_id = r.action_id.clone().expect("composed");

    // Owner-style compose on the preferred channel (the iMessage handle).
    assert_eq!(
        fx.composer.composes(),
        vec![(format!("imessage:{PHONE}"), text.to_string())]
    );
    // Inbound-only checks are skipped (no receipt, no triage, no inbound)
    // and the same approve path the Approve button calls ran once.
    assert_eq!(fx.exec.approvals(), vec![action_id.clone()]);
    assert_eq!(outbox_bodies(&fx.store), vec![(PHONE.into(), text.into())]);
    assert!(fx.inner.cards().is_empty(), "auto-sent: no card");

    // D11 record: proactive thread key, no inbound, one executed send step.
    let plan = fx
        .store
        .autonomous_plan(r.plan_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(plan.thread_key, format!("{PROACTIVE_THREAD_PREFIX}{SLUG}"));
    assert_eq!(plan.inbound_message_id, None);
    assert_eq!(plan.action_id, action_id);
    assert_eq!(plan.tier, "full");
    assert_eq!(plan.status, PlanStatus::Executed);
    let json: serde_json::Value = serde_json::from_str(&plan.plan_json).unwrap();
    assert_eq!(json["plan"], "outreach");
    assert_eq!(json["why"], "deck is due today");
    assert_eq!(json["trigger"], "proactive");
    let steps = fx.store.autonomous_steps(plan.id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].target_handle, format!("imessage:{PHONE}"));

    // D10 notice with the exact text sent.
    let notices = fx.inner.notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        augmentagent_approval_discord::autonomy::step_text(&notices[0].steps[0]).as_deref(),
        Some(text)
    );

    // The content floor still applies: composed, carded with the reason,
    // never approved.
    let fx = fx_with(snapshot(full()), true);
    let r = send(&fx, "Can you wire me $500 for the venue?", now_ms()).await;
    assert_eq!(r.decision, OutreachDecision::NeedsApproval, "{r:?}");
    assert_eq!(r.reason, Some(Reason::ContentFloor));
    assert!(fx.exec.approvals().is_empty());
    let cards = noted(&fx);
    assert_eq!(cards.len(), 1);
    assert_eq!(Some(cards[0].0.clone()), r.action_id);
    assert!(cards[0].1.contains("content_floor"), "{}", cards[0].1);
    assert!(outbox_bodies(&fx.store).is_empty());

    // Paused: card, no send.
    let fx = fx_with(snapshot(full()), true);
    fx.store.pause_contact(SLUG, now_ms()).unwrap();
    let r = send(&fx, "Lunch Friday?", now_ms()).await;
    assert_eq!(r.reason, Some(Reason::ContactPaused));
    assert_eq!(r.decision, OutreachDecision::NeedsApproval);
    assert!(fx.exec.approvals().is_empty());

    // Daily cap: card once the cap is reached.
    let mut capped = full();
    capped.daily_cap = 1;
    let fx = fx_with(snapshot(capped), true);
    let t0 = window_start();
    assert_eq!(
        send(&fx, "first", t0).await.decision,
        OutreachDecision::AutoSent
    );
    let r = send(&fx, "second", t0 + 31 * MIN).await;
    assert_eq!(r.reason, Some(Reason::DailyCap), "{r:?}");
    assert_eq!(r.decision, OutreachDecision::NeedsApproval);
    assert_eq!(fx.exec.approvals().len(), 1);

    // Master switch off or pause-all: nothing composed, nothing carded.
    let fx = fx_with(snapshot(full()), false);
    let r = send(&fx, "Lunch Friday?", now_ms()).await;
    assert_eq!(
        (r.decision, r.reason),
        (OutreachDecision::Refused, Some(Reason::AutonomyOff))
    );
    let fx2 = fixture();
    fx2.store.set_autonomy_global_pause(true, now_ms()).unwrap();
    let r2 = send(&fx2, "Lunch Friday?", now_ms()).await;
    assert_eq!(r2.reason, Some(Reason::AutonomyOff));
    for f in [&fx, &fx2] {
        assert!(f.composer.composes().is_empty());
        assert!(f.inner.cards().is_empty());
    }
}

#[tokio::test]
async fn same_text_within_24h_deduped() {
    let fx = fixture();
    let t0 = window_start();
    assert_eq!(
        send(&fx, "Can you send the deck?", t0).await.decision,
        OutreachDecision::AutoSent
    );
    // Same text (modulo case and spacing), a later 30-min window: deduped
    // by the 24 h text check, nothing composed, no card.
    let r = send(&fx, "  can you SEND   the deck? ", t0 + 31 * MIN).await;
    assert_eq!(
        (r.decision.clone(), r.reason),
        (OutreachDecision::Refused, Some(Reason::Duplicate)),
        "{r:?}"
    );
    assert_eq!(fx.composer.composes().len(), 1);
    assert!(fx.inner.cards().is_empty());

    // Different text, a fresh window: goes.
    assert_eq!(
        send(&fx, "Also: dinner at 7?", t0 + 60 * MIN)
            .await
            .decision,
        OutreachDecision::AutoSent
    );
    // Different text in the SAME 30-min window: the per-thread idempotency
    // key (`proactive:<slug>`) holds it.
    let r = send(&fx, "One more thing", t0 + 61 * MIN).await;
    assert_eq!(r.reason, Some(Reason::Duplicate), "{r:?}");
    assert_eq!(fx.composer.composes().len(), 2);

    // A held (carded) proposal counts too: the owner is not asked twice.
    let fx2 = fixture();
    fx2.store.pause_contact(SLUG, t0).unwrap();
    assert_eq!(
        send(&fx2, "Lunch Friday?", t0).await.decision,
        OutreachDecision::NeedsApproval
    );
    let r = send(&fx2, "lunch friday?", t0 + 31 * MIN).await;
    assert_eq!(r.reason, Some(Reason::Duplicate));
    assert_eq!(noted(&fx2).len(), 1);

    // After 24 h the same text may go again.
    assert_eq!(
        send(&fx, "Can you send the deck?", t0 + 25 * HOUR)
            .await
            .decision,
        OutreachDecision::AutoSent
    );
    assert_eq!(fx.exec.approvals().len(), 3);
}

#[tokio::test]
async fn owner_messaged_thread_recently_blocks() {
    // The owner texted Jane 30 minutes ago: card, no send.
    let fx = fixture();
    owner_texted(&fx.store, PHONE, 1, &minutes_ago(30));
    let r = send(&fx, "Running late, start without me", now_ms()).await;
    assert_eq!(r.reason, Some(Reason::OwnerTookOver), "{r:?}");
    assert_eq!(r.decision, OutreachDecision::NeedsApproval);
    assert!(fx.exec.approvals().is_empty());
    let cards = noted(&fx);
    assert_eq!(cards.len(), 1);
    assert!(cards[0].1.contains("owner_took_over"));

    // Three hours ago is outside the 2 h window: it goes.
    let fx = fixture();
    owner_texted(&fx.store, PHONE, 1, &minutes_ago(180));
    let r = send(&fx, "Running late, start without me", now_ms()).await;
    assert_eq!(r.decision, OutreachDecision::AutoSent, "{r:?}");
}

#[tokio::test]
async fn unknown_or_actions_tier_slug_dropped() {
    // The engine refuses (defence in depth behind the decision parser):
    // nothing composed, nothing carded.
    let fx = fx_with(mixed_snapshot(), true);
    let r = fx
        .engine
        .run(ACTIONS_SLUG, "hello", "why", false, now_ms())
        .await;
    assert_eq!(
        (r.decision, r.reason),
        (OutreachDecision::Refused, Some(Reason::ActionNotAllowed))
    );
    for slug in ["nobody-here", PENDING_SLUG] {
        let r = fx.engine.run(slug, "hello", "why", false, now_ms()).await;
        assert_eq!(
            (r.decision, r.reason),
            (OutreachDecision::Refused, Some(Reason::NotTrusted)),
            "{slug}"
        );
    }
    assert!(fx.composer.composes().is_empty());
    assert!(fx.inner.cards().is_empty());
    assert!(fx.exec.approvals().is_empty());
}

#[tokio::test]
async fn dry_run_shows_the_decision_without_side_effects() {
    let fx = fixture();
    let r = fx
        .engine
        .run(SLUG, "Can you send the deck?", "due", true, now_ms())
        .await;
    assert_eq!(r.decision, OutreachDecision::WouldAutoSend, "{r:?}");
    assert_eq!(
        r.handle.as_deref(),
        Some(format!("imessage:{PHONE}").as_str())
    );
    assert!(r.idem_key.is_some());
    assert_eq!(fx.composer.checks().len(), 1);
    assert!(fx.composer.composes().is_empty());
    assert!(fx.exec.approvals().is_empty());
    assert!(fx.inner.calls().iter().all(|c| matches!(c, Call::Other(_))));
    assert_eq!(
        fx.store
            .autonomous_plans_in_cap_window(SLUG, now_ms())
            .unwrap(),
        0
    );
    // Nothing logged: the same text is not a duplicate afterwards.
    let r = fx
        .engine
        .run(SLUG, "Can you send the deck?", "due", true, now_ms())
        .await;
    assert_eq!(r.decision, OutreachDecision::WouldAutoSend);

    let r = fx
        .engine
        .run(SLUG, "Your password is hunter2", "x", true, now_ms())
        .await;
    assert_eq!(
        (r.decision, r.reason),
        (
            OutreachDecision::WouldNeedApproval,
            Some(Reason::ContentFloor)
        )
    );
}

// ----- the heartbeat runner wired to the engine ----------------------------

struct Scripted {
    reply: String,
    prompts: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl Reasoner for Scripted {
    async fn call(&self, opts: &ReasonerOpts, user: &str) -> anyhow::Result<String> {
        self.prompts
            .lock()
            .unwrap()
            .push((opts.system_prompt.clone(), user.to_string()));
        Ok(self.reply.clone())
    }
}

fn heartbeat_for(fx: &Fx, reply: &str) -> (HeartbeatRunner<Scripted>, Arc<Scripted>) {
    std::fs::write(
        fx.wiki.join("HEARTBEAT.md"),
        "- nudge people about the deck\n",
    )
    .unwrap();
    let reasoner = Arc::new(Scripted {
        reply: reply.into(),
        prompts: Mutex::new(Vec::new()),
    });
    let config = HeartbeatConfig {
        enabled: true,
        tz: Some(chrono_tz::UTC),
        timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let runner = HeartbeatRunner::new(
        Arc::clone(&fx.store),
        Arc::clone(&fx.inner) as Arc<dyn ApprovalBroker>,
        Arc::clone(&reasoner),
        fx.wiki.clone(),
        config,
    )
    .with_holder("test")
    .with_outreach(Arc::clone(&fx.engine) as Arc<dyn Outreach>);
    (runner, reasoner)
}

const FORCE: RunOptions = RunOptions {
    force: true,
    dry_run: false,
};

#[tokio::test]
async fn prompt_lists_only_full_tier_slugs() {
    let fx = fx_with(mixed_snapshot(), true);
    let roster = fx.engine.roster(now_ms());
    assert_eq!(roster.len(), 1, "{roster:?}");
    assert_eq!(roster[0].slug, SLUG);
    assert_eq!(roster[0].display_name, "Jane Doe");

    let (runner, reasoner) = heartbeat_for(&fx, r#"{"notify": false}"#);
    runner.run_once(FORCE).await.unwrap();
    let prompts = reasoner.prompts.lock().unwrap();
    let (system, user) = &prompts[0];
    assert!(system.contains("outreach"), "the contract is explained");
    assert!(user.contains(SLUG) && user.contains("Jane Doe"), "{user}");
    for absent in [
        ACTIONS_SLUG,
        PENDING_SLUG,
        PHONE,
        ACTIONS_PHONE,
        PENDING_PHONE,
        WORK_EMAIL,
    ] {
        assert!(!user.contains(absent), "{absent} leaked into the prompt");
        assert!(!system.contains(absent), "{absent} leaked into the prompt");
    }
}

#[tokio::test]
async fn invalid_decision_never_outreaches() {
    let entry =
        format!(r#"[{{"contact": "{SLUG}", "message": "Can you send the deck?", "why": "due"}}]"#);
    // Invalid decision: nothing composed, nothing sent.
    let fx = fixture();
    let (runner, _) = heartbeat_for(&fx, &format!(r#"{{"notify": true, "outreach": {entry}}}"#));
    let report = runner.run_once(FORCE).await.unwrap();
    assert_eq!(report.decision.as_deref(), Some("invalid"));
    assert!(report.outreach.is_empty());
    assert!(fx.composer.composes().is_empty());
    assert!(fx.exec.approvals().is_empty());

    // The same entry on a valid decision goes through the gate.
    let fx = fixture();
    let (runner, _) = heartbeat_for(&fx, &format!(r#"{{"notify": false, "outreach": {entry}}}"#));
    let report = runner.run_once(FORCE).await.unwrap();
    assert_eq!(report.decision.as_deref(), Some("silent"));
    assert_eq!(report.outreach.len(), 1, "{report:?}");
    assert_eq!(report.outreach[0].status, "auto-sent");
    assert_eq!(fx.exec.approvals().len(), 1);

    // A heartbeat dry run proposes but changes nothing.
    let fx = fixture();
    let (runner, _) = heartbeat_for(&fx, &format!(r#"{{"notify": false, "outreach": {entry}}}"#));
    let report = runner
        .run_once(RunOptions {
            force: true,
            dry_run: true,
        })
        .await
        .unwrap();
    assert_eq!(report.outreach[0].status, "would-auto-send");
    assert!(fx.composer.composes().is_empty());
    assert!(fx.exec.approvals().is_empty());
}
