use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use augmentagent_channel_core::native_session::CURRENT;
use augmentagent_channel_core::surface_turn::{run_surface_turn, SurfaceTurnOutcome, SurfaceTurnRequest};
use augmentagent_store::{Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef, SurfaceTurnResolution};
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

#[derive(Default)]
struct Capture(Mutex<Vec<BTreeMap<String, String>>>);

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Fields {
    fn insert(&mut self, field: &Field, value: impl ToString) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.insert(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, value);
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.insert(field, value);
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool { true }
    fn new_span(&self, _: &Attributes<'_>) -> Id { Id::from_u64(1) }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.contains_key("message") {
            self.0.lock().unwrap().push(fields.0);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn conversation(platform: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), "autoship-account").unwrap(),
        "autoship-conversation",
        None,
    ).unwrap()
}

fn request<'a>(turn: &'a SurfaceTurnRef) -> SurfaceTurnRequest<'a> {
    SurfaceTurnRequest { turn, history: "", current: "observe", cwd: "/fixture" }
}

fn complete_event(event: Option<&BTreeMap<String, String>>, fields: &[&str]) -> bool {
    let Some(event) = event else { return false; };
    fields.iter().all(|field| event.contains_key(*field))
        && event.get("budget_consumer").is_some_and(|value| !value.trim_matches('"').trim().is_empty())
}

fn same_receipt(events: &BTreeMap<String, BTreeMap<String, String>>, dispatched: &str, completed: &str) -> bool {
    events.get(dispatched).and_then(|event| event.get("received_at_ms"))
        .zip(events.get(completed).and_then(|event| event.get("received_at_ms")))
        .is_some_and(|(left, right)| left == right)
}

fn main() {
    let root = std::env::temp_dir().join(format!(
        "autoship-surface-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(root.join("data.db")).unwrap();
    let capture = Arc::new(Capture::default());
    let dispatch = tracing::Dispatch::new(Arc::clone(&capture));

    let observed = tracing::dispatcher::with_default(&dispatch, || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut native_scope = false;
        for platform in ["discord", "slack"] {
            let turn = SurfaceTurnRef::new(conversation(platform), format!("timing-{platform}")).unwrap();
            let outcome = runtime.block_on(run_surface_turn(
                &store,
                request(&turn),
                || Ok(None),
                None,
                |_| async {
                    let scoped = CURRENT.try_with(|_| ()).is_ok();
                    Ok(if scoped { "scoped".to_string() } else { "unscoped".to_string() })
                },
            )).unwrap();
            native_scope |= matches!(outcome, SurfaceTurnOutcome::Answered(ref answer) if answer == "scoped");
        }

        let claim_turn = SurfaceTurnRef::new(conversation("slack"), "duplicate-claim").unwrap();
        let first_claim = runtime.block_on(run_surface_turn(
            &store, request(&claim_turn), || Ok(None), None, |_| async { Ok("ok".to_string()) },
        )).is_ok();
        let duplicate_rejected = runtime.block_on(run_surface_turn(
            &store, request(&claim_turn), || Ok(None), None, |_| async { Ok("again".to_string()) },
        )).is_err();

        let cancel = CancellationToken::new();
        let cancel_turn = SurfaceTurnRef::new(conversation("slack"), "cancelled-claim").unwrap();
        let cancellation = runtime.block_on(run_surface_turn(
            &store,
            request(&cancel_turn),
            || Ok(None),
            Some(&cancel),
            |_| {
                let cancel = cancel.clone();
                async move {
                    cancel.cancel();
                    std::future::pending::<anyhow::Result<String>>().await
                }
            },
        ));
        let cancellation_preserved = matches!(cancellation, Ok(SurfaceTurnOutcome::Cancelled))
            && store.surface_turn_state(&cancel_turn).unwrap().unwrap().resolution == Some(SurfaceTurnResolution::Cancelled);

        let mut events = BTreeMap::new();
        for fields in capture.0.lock().unwrap().iter() {
            if let Some(message) = fields.get("message") {
                events.insert(message.trim_matches('"').to_string(), fields.clone());
            }
        }
        let dispatched = ["received_at_ms", "budget_consumer", "handler_dispatched_at_ms"];
        let completed = ["received_at_ms", "budget_consumer", "handler_dispatched_at_ms", "answer_completed_at_ms", "native_submitted_at_ms", "first_text_output_at_ms"];
        [
            complete_event(events.get("Discord native turn handler dispatched"), &dispatched),
            complete_event(events.get("Discord native turn timing"), &completed),
            complete_event(events.get("surface native turn handler dispatched"), &dispatched),
            complete_event(events.get("surface native turn timing"), &completed),
            same_receipt(&events, "Discord native turn handler dispatched", "Discord native turn timing")
                && same_receipt(&events, "surface native turn handler dispatched", "surface native turn timing"),
            native_scope,
            first_claim && duplicate_rejected && cancellation_preserved,
        ]
    });

    println!("{}", serde_json::to_string(&observed).unwrap());
    let _ = std::fs::remove_dir_all(root);
}
