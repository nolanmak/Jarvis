//! Jarvis Dashboard socket ops (spec web-dashboard-v2 §5.2): thread and turn
//! writes, each mapped to the store / runner and to the §5.2.1 codes. The
//! daemon is the only writer (D3); the dashboard reads on its own.
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalActionHandler, ApprovalActionOutcome};
use augmentagent_store::web_chat::{WebChatError, WebThread};
use augmentagent_store::Store;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::web_surface_transport::{socket_path, Listener, OpError, WebOps};
use crate::web_turn_runner::WebTurnRunner;

pub struct WebSurface {
    store: Arc<Store>,
    runner: Arc<WebTurnRunner>,
    /// `SyncingActionHandler::new("web", …)` around serve's approver; `None`
    /// when serve has no approver (approvals then answer `internal`).
    actions: Option<Arc<dyn ApprovalActionHandler>>,
}

#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
enum Op {
    #[serde(rename = "thread.create")]
    ThreadCreate { title: Option<String> },
    #[serde(rename = "thread.rename")]
    ThreadRename { thread_id: String, title: String },
    #[serde(rename = "thread.set")]
    ThreadSet {
        thread_id: String,
        pinned: Option<bool>,
        archived: Option<bool>,
    },
    #[serde(rename = "thread.reset_session")]
    ThreadResetSession { thread_id: String },
    #[serde(rename = "turn.submit")]
    TurnSubmit {
        thread_id: String,
        turn_id: String,
        text: String,
    },
    #[serde(rename = "turn.cancel")]
    TurnCancel { turn_id: String },
    #[serde(rename = "action.resolve")]
    ActionResolve {
        action_id: String,
        verb: String,
        note: Option<String>,
    },
}

/// The thread object the dashboard client expects (`WebThread` in
/// `src/web/daemonClient.ts`).
pub fn thread_json(thread: &WebThread) -> Value {
    json!({
        "id": thread.id,
        "title": thread.title,
        "created_at": thread.created_at,
        "updated_at": thread.updated_at,
        "changed_at": thread.changed_at,
        "pinned": thread.pinned,
        "archived": thread.archived,
    })
}

fn refusal(error: WebChatError) -> OpError {
    match error {
        WebChatError::Store(inner) => {
            tracing::error!(error = %inner, "web surface store error");
            OpError::new("internal", "Jarvis could not complete the request")
        }
        other => OpError::new(other.code(), other.to_string()),
    }
}

impl WebSurface {
    pub fn new(
        store: Arc<Store>,
        runner: Arc<WebTurnRunner>,
        actions: Option<Arc<dyn ApprovalActionHandler>>,
    ) -> Self {
        Self {
            store,
            runner,
            actions,
        }
    }

    fn run(&self, op: Op) -> Result<Value, WebChatError> {
        let thread = |t: WebThread| json!({"thread": thread_json(&t)});
        Ok(match op {
            Op::ThreadCreate { title } => thread(self.store.web_create_thread(title.as_deref())?),
            Op::ThreadRename { thread_id, title } => {
                thread(self.store.web_rename_thread(&thread_id, &title)?)
            }
            Op::ThreadSet {
                thread_id,
                pinned,
                archived,
            } => thread(self.store.web_set_thread(&thread_id, pinned, archived)?),
            Op::ThreadResetSession { thread_id } => thread(self.runner.reset_session(&thread_id)?),
            Op::TurnSubmit {
                thread_id,
                turn_id,
                text,
            } => {
                let submitted = self.runner.submit(&thread_id, &turn_id, &text)?;
                json!({"turn_id": turn_id, "status": submitted.status.as_str()})
            }
            Op::TurnCancel { turn_id } => {
                let status = self.runner.cancel(&turn_id)?;
                json!({"turn_id": turn_id, "status": status.as_str()})
            }
            Op::ActionResolve { .. } => unreachable!("action.resolve is dispatched async"),
        })
    }
}

#[async_trait]
impl WebOps for WebSurface {
    async fn dispatch(&self, op: &str, mut args: Value) -> Result<Value, OpError> {
        if let Value::Object(fields) = &mut args {
            fields.insert("op".into(), Value::String(op.into()));
        }
        let op: Op = serde_json::from_value(args)
            .map_err(|_| OpError::new("bad_request", "unknown op or invalid arguments"))?;
        if let Op::ActionResolve {
            action_id,
            verb,
            note,
        } = op
        {
            let Some(actions) = &self.actions else {
                return Err(OpError::new("internal", "approvals are not available"));
            };
            return resolve_action(actions.as_ref(), &action_id, &verb, note.as_deref()).await;
        }
        self.run(op).map_err(refusal)
    }
}

/// `action.resolve` (§5.2.2): approve, skip or revise (with a non-empty
/// note) through the real handler; exactly-once is the store's CAS.
pub async fn resolve_action(
    actions: &dyn ApprovalActionHandler,
    action_id: &str,
    verb: &str,
    note: Option<&str>,
) -> Result<Value, OpError> {
    if action_id.is_empty() {
        return Err(OpError::new("bad_request", "action_id is required"));
    }
    let outcome = match verb {
        "approve" => actions.approve(action_id).await,
        "skip" => actions.skip(action_id).await,
        "revise" => match note.map(str::trim).filter(|n| !n.is_empty()) {
            Some(note) => actions.revise(action_id, note).await,
            None => return Err(OpError::new("bad_request", "revise needs a note")),
        },
        _ => {
            return Err(OpError::new(
                "bad_request",
                "verb must be approve, skip or revise",
            ))
        }
    };
    web_outcome_json(&outcome).map(|outcome| json!({ "outcome": outcome }))
}

/// The one mapping from a handler outcome to the dashboard's
/// `{kind, summary}` (plus `text`/`link`, `local` or `message` where §5.2.2
/// allows). `AlreadyResolved` and `NotFound` are refusals.
pub fn web_outcome_json(outcome: &ApprovalActionOutcome) -> Result<Value, OpError> {
    use ApprovalActionOutcome as O;
    let plain = |kind: &str, summary: String| Ok(json!({"kind": kind, "summary": summary}));
    match outcome {
        O::NotFound => Err(OpError::new("action_not_found", "no such action")),
        O::AlreadyResolved { status, .. } => Err(OpError::new(
            "already_resolved",
            format!("already resolved ({status})"),
        )),
        O::Approved => plain("approved", "Approved and sent.".into()),
        O::CalendarCreated {
            already_existed, ..
        } => plain(
            "calendar_created",
            if *already_existed {
                "The calendar event already existed.".into()
            } else {
                "Calendar event created.".into()
            },
        ),
        O::CalendarResponded {
            response, summary, ..
        } => plain(
            "calendar_responded",
            format!("Responded {response} to {summary}."),
        ),
        O::CalendarRescheduled {
            summary,
            when,
            already_there,
            ..
        } => plain(
            "calendar_rescheduled",
            if *already_there {
                format!("{summary} was already at {when}.")
            } else {
                format!("{summary} moved to {when}.")
            },
        ),
        O::DocumentSigned { .. } => plain("document_signed", "Document signed.".into()),
        O::ManualSendReady { text, link } => Ok(json!({
            "kind": "manual_send_ready",
            "summary": "Not sent: send this yourself.",
            "text": text,
            "link": link,
        })),
        O::Skipped => plain("skipped", "Skipped.".into()),
        O::Revised { .. } => plain(
            "revised",
            "Revised: a new draft is waiting for approval.".into(),
        ),
        O::Scheduled { local, .. } => Ok(json!({
            "kind": "scheduled",
            "summary": format!("Scheduled for {local}."),
            "local": local,
        })),
        O::Unscheduled => plain("unscheduled", "Back in the queue.".into()),
        O::CancelledSchedule => plain("cancelled_schedule", "Scheduled send cancelled.".into()),
        O::Recomposed => plain("recomposed", "Draft restored for approval.".into()),
        O::Failed { message } => Ok(json!({
            "kind": "failed",
            "summary": "Failed: the action is still pending.",
            "message": message,
        })),
    }
}

/// `serve` startup: with `AUGMENTAGENT_WEB_SOCKET` set, sweep interrupted web
/// turns, then open the socket. Unset → disabled, logged once.
pub fn start(
    wiki: Option<&PathBuf>,
    store: Arc<Store>,
    query: impl FnOnce(PathBuf) -> Arc<dyn augmentagent_approval_discord::QueryHandler>,
    actions: Option<Arc<dyn ApprovalActionHandler>>,
    stop: CancellationToken,
) -> Option<tokio::task::JoinHandle<anyhow::Result<()>>> {
    let Some(value) = std::env::var_os("AUGMENTAGENT_WEB_SOCKET") else {
        tracing::info!("Jarvis Dashboard web surface disabled: AUGMENTAGENT_WEB_SOCKET is unset");
        return None;
    };
    let path = match socket_path(&value) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!("Jarvis Dashboard web surface disabled: {error:#}");
            return None;
        }
    };
    let Some(wiki) = wiki else {
        tracing::warn!("Jarvis Dashboard web surface disabled: no wiki directory");
        return None;
    };
    match store.web_restart_sweep() {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            turns = n,
            "web turns interrupted by a restart marked failed"
        ),
        Err(error) => {
            tracing::error!("Jarvis Dashboard web surface disabled: restart sweep failed: {error}");
            return None;
        }
    }
    let listener = match Listener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("Jarvis Dashboard web surface disabled: {error:#}");
            return None;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let handler = query(wiki.clone());
    let runner = WebTurnRunner::new(Arc::clone(&store), handler, cwd.display().to_string());
    let surface = Arc::new(WebSurface::new(store, runner, actions));
    tracing::info!(socket = %path.display(), "Jarvis Dashboard web surface listening");
    Some(tokio::spawn(async move {
        if let Err(error) = listener.serve(surface, stop.clone()).await {
            tracing::warn!("Jarvis Dashboard web surface stopped: {error:#}");
            // An optional listener failing must not stop the daemon.
            stop.cancelled().await;
        }
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface_conformance_tests::{isolated, querier};
    use crate::web_surface_transport::respond;
    use augmentagent_channel_core::providers::ProviderKind;
    use augmentagent_channel_core::surface_conformance::RecordingNativeProvider;

    async fn call(surface: &WebSurface, request: Value) -> Value {
        respond(request.to_string().as_bytes(), surface).await
    }

    #[tokio::test]
    async fn web_surface_round_trips_every_op() {
        let Some(root) = isolated(
            "web_surface::tests::web_surface_round_trips_every_op",
            "WEB_SURFACE_OPS_ROOT",
        ) else {
            return;
        };
        let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
        let handler = querier(&root, &provider);
        let store = handler.conversation_store.clone().unwrap();
        let runner = WebTurnRunner::new(
            Arc::clone(&store),
            Arc::new(handler),
            root.display().to_string(),
        );
        let surface = WebSurface::new(Arc::clone(&store), runner, None);

        let created = call(&surface, json!({"v":1,"op":"thread.create"})).await;
        assert_eq!(created["ok"], true);
        let thread = created["thread"].clone();
        let keys: Vec<&String> = thread.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "archived",
                "changed_at",
                "created_at",
                "id",
                "pinned",
                "title",
                "updated_at"
            ]
        );
        assert_eq!(thread["title"], "New thread");
        assert_eq!(thread["pinned"], false);
        let id = thread["id"].as_str().unwrap().to_string();

        let titled = call(
            &surface,
            json!({"v":1,"op":"thread.create","title":"Plans"}),
        )
        .await;
        assert_eq!(titled["thread"]["title"], "Plans");
        let renamed = call(
            &surface,
            json!({"v":1,"op":"thread.rename","thread_id":id,"title":"Trip"}),
        )
        .await;
        assert_eq!(renamed["thread"]["title"], "Trip");
        let set = call(
            &surface,
            json!({"v":1,"op":"thread.set","thread_id":id,"pinned":true}),
        )
        .await;
        assert_eq!(
            (
                set["thread"]["pinned"].clone(),
                set["thread"]["archived"].clone()
            ),
            (json!(true), json!(false))
        );

        let submit = json!({"v":1,"op":"turn.submit","thread_id":id,"turn_id":"t1","text":"hello"});
        let submitted = call(&surface, submit.clone()).await;
        assert_eq!(
            submitted,
            json!({"ok":true,"turn_id":"t1","status":"queued"})
        );
        for _ in 0..500 {
            if store.web_turn("t1").unwrap().status.as_str() == "done" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            call(&surface, submit).await["status"],
            "done",
            "idempotent replay"
        );
        let cancel = call(&surface, json!({"v":1,"op":"turn.cancel","turn_id":"t1"})).await;
        assert_eq!(cancel, json!({"ok":true,"turn_id":"t1","status":"done"}));
        let reset = call(
            &surface,
            json!({"v":1,"op":"thread.reset_session","thread_id":id}),
        )
        .await;
        assert_eq!(reset["thread"]["id"], id.as_str());

        let code = |v: Value| v["code"].as_str().unwrap().to_string();
        let changed =
            json!({"v":1,"op":"turn.submit","thread_id":id,"turn_id":"t1","text":"other"});
        assert_eq!(code(call(&surface, changed).await), "turn_conflict");
        let missing = json!({"v":1,"op":"thread.rename","thread_id":"nope","title":"x"});
        assert_eq!(code(call(&surface, missing).await), "thread_not_found");
        let big = json!({"v":1,"op":"turn.submit","thread_id":id,"turn_id":"t2","text":"x".repeat(32 * 1024 + 1)});
        assert_eq!(code(call(&surface, big).await), "text_too_large");
        let gone = json!({"v":1,"op":"turn.cancel","turn_id":"nope"});
        assert_eq!(code(call(&surface, gone).await), "turn_not_found");
        let extra = json!({"v":1,"op":"thread.create","colour":"red"});
        assert_eq!(code(call(&surface, extra).await), "bad_request");
        let archived = json!({"v":1,"op":"thread.set","thread_id":id,"archived":true});
        call(&surface, archived).await;
        let late = json!({"v":1,"op":"turn.submit","thread_id":id,"turn_id":"t3","text":"hi"});
        assert_eq!(code(call(&surface, late).await), "thread_archived");
    }
}
