//! #1833 C10 (D16) — outreach needs both switches. Its own test binary: it
//! mutates the process environment, so nothing else may run beside it.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_autonomy::gate::{AutonomyGate, GateDeps};
use augmentagent_autonomy::outreach::{
    heartbeat_outreach, Composed, OutreachComposer, OutreachEngine,
};
use augmentagent_autonomy::{Handle, Tier};
use common::*;

struct NoCompose;

#[async_trait]
impl OutreachComposer for NoCompose {
    async fn check(&self, _: &Handle, _: &str) -> Result<String, String> {
        Err("test".into())
    }
    async fn compose(&self, _: &Handle, _: &str) -> Result<Composed, String> {
        Err("test".into())
    }
}

#[test]
fn outreach_off_unless_both_switches_on() {
    let (_dir, store) = store();
    let engine = Arc::new(OutreachEngine {
        gate: Arc::new(AutonomyGate::new(GateDeps {
            store,
            master_on: true,
            trust: Arc::new(snapshot(contact(Tier::Full, &[]))),
            calendar: None,
            planner: None,
            owner_addresses: Vec::new(),
            tz: chrono_tz::UTC,
        })),
        broker: None,
        composer: Arc::new(NoCompose),
        wiki_root: None,
        notices_deliverable: true,
    });
    let cases = [
        (None, None, false),
        (Some("on"), None, false),
        (None, Some("on"), false),
        (Some("on"), Some("off"), false),
        (Some("off"), Some("on"), false),
        (Some("on"), Some("true"), false),
        (Some("true"), Some("on"), false),
        (Some("on"), Some("on"), true),
        (Some(" ON "), Some("On"), true),
    ];
    for (master, outreach, expected) in cases {
        match master {
            Some(v) => std::env::set_var("AUGMENTAGENT_AUTONOMY", v),
            None => std::env::remove_var("AUGMENTAGENT_AUTONOMY"),
        }
        match outreach {
            Some(v) => std::env::set_var("AUGMENTAGENT_HEARTBEAT_OUTREACH", v),
            None => std::env::remove_var("AUGMENTAGENT_HEARTBEAT_OUTREACH"),
        }
        assert_eq!(
            heartbeat_outreach(Arc::clone(&engine)).is_some(),
            expected,
            "AUGMENTAGENT_AUTONOMY={master:?} AUGMENTAGENT_HEARTBEAT_OUTREACH={outreach:?}"
        );
    }
}
