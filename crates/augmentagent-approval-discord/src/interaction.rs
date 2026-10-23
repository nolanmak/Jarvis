//! Trusted transport metadata follows the handler through spawned tasks.
use crate::{ApprovalActionHandler, ApprovalActionOutcome};
pub use augmentagent_store::approval_history::DecisionContext;
use std::{future::Future, sync::Arc};
tokio::task_local! { static CURRENT: DecisionContext; static HISTORY: bool; }
pub fn current() -> Option<DecisionContext> {
    CURRENT.try_with(Clone::clone).ok()
}
pub async fn deciding<F: Future>(ctx: DecisionContext, f: F) -> F::Output {
    CURRENT.scope(ctx, f).await
}
pub async fn owner_history<F: Future>(allowed: bool, f: F) -> F::Output {
    HISTORY.scope(allowed, f).await
}
pub fn history_allowed() -> bool {
    HISTORY.try_with(|v| *v).unwrap_or(false)
}
struct Contextual {
    inner: Arc<dyn ApprovalActionHandler>,
    ctx: DecisionContext,
}
pub fn contextual(
    inner: Option<Arc<dyn ApprovalActionHandler>>,
    ctx: DecisionContext,
) -> Option<Arc<dyn ApprovalActionHandler>> {
    inner.map(|inner| Arc::new(Contextual { inner, ctx }) as Arc<dyn ApprovalActionHandler>)
}
#[async_trait::async_trait]
impl ApprovalActionHandler for Contextual {
    async fn approve(&self, id: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.approve(id)).await
    }
    async fn skip(&self, id: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.skip(id)).await
    }
    async fn revise(&self, id: &str, feedback: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.revise(id, feedback)).await
    }
    async fn schedule(&self, id: &str, at_ms: i64) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.schedule(id, at_ms)).await
    }
    async fn reschedule(&self, id: &str, at_ms: i64) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.reschedule(id, at_ms)).await
    }
    async fn send_now(&self, id: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.send_now(id)).await
    }
    async fn cancel_schedule(&self, id: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.cancel_schedule(id)).await
    }
    async fn back_to_queue(&self, id: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.back_to_queue(id)).await
    }
    async fn recompose(&self, id: &str) -> ApprovalActionOutcome {
        deciding(self.ctx.clone(), self.inner.recompose(id)).await
    }
    async fn is_resolved(&self, id: &str) -> bool {
        self.inner.is_resolved(id).await
    }
    async fn is_schedule_live(&self, id: &str) -> bool {
        self.inner.is_schedule_live(id).await
    }
}

pub fn draft_revision(draft: &str) -> String {
    use sha2::{Digest, Sha256};
    let (human, needs) = crate::split_needs_input(draft);
    let (prose, _) = crate::layout::split_trailing_envelope_markers(&human);
    let canonical = serde_json::to_vec(&(
        prose.trim_end(),
        needs.iter().map(|n| (&n.kind, &n.text)).collect::<Vec<_>>(),
    ))
    .unwrap_or_default();
    format!("{:x}", Sha256::digest(canonical))[..16].to_string()
}

/// Local-only controls use the same durable decision/unknown-outcome fence.
/// The transport must authorize the actor before entering here.
pub fn local_decision<T>(
    store: &augmentagent_store::Store,
    ctx: &DecisionContext,
    id: &str,
    verb: &str,
    kind: &str,
    summary: &str,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let seq = store
        .begin_approval(id, ctx, verb, "local-control", kind, summary)?
        .ok_or_else(|| {
            anyhow::anyhow!("Decision already recorded or unconfirmed; no repeat operation")
        })?;
    match f() {
        Ok(value) => {
            store.finish_approval(
                seq,
                "completed",
                "Local control applied; no outbound reply sent",
            )?;
            Ok(value)
        }
        Err(error) => {
            store.finish_approval(
                seq,
                "unconfirmed",
                "Local control interrupted; check its persisted state before retrying",
            )?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_controls_record_once_and_do_not_send() {
        let d = tempfile::tempdir().unwrap();
        let s = augmentagent_store::Store::open(d.path().join("db")).unwrap();
        let ctx = DecisionContext {
            surface: "discord".into(),
            actor: "owner".into(),
            conversation: "private".into(),
            interaction_id: "click".into(),
            revision: None,
        };
        let result = local_decision(&s, &ctx, "alert1", "ack", "owner_alert", "Alert", || {
            Ok("applied")
        });
        assert_eq!(result.unwrap(), "applied");
        assert!(local_decision::<()>(
            &s,
            &ctx,
            "alert1",
            "ack",
            "owner_alert",
            "Alert",
            || panic!("duplicate effect")
        )
        .is_err());
        let rows = s.approval_history(None, 20, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verb, "ack");
        assert_eq!(rows[0].actor, "owner");
    }
    #[test]
    fn draft_fingerprint_survives_card_decoration_and_changes_with_content() {
        let draft = "Hello,\nHere is the proposal.";
        assert_eq!(
            draft_revision(draft),
            draft_revision(&format!(
                "{draft}\n\n[to: reader@example.test]\n[attachment: proposal.pdf]"
            ))
        );
        assert_ne!(draft_revision(draft), draft_revision("Updated proposal"));
    }
    #[tokio::test]
    async fn history_permission_is_task_scoped_and_does_not_leak() {
        assert!(!history_allowed());
        owner_history(true, async {
            assert!(history_allowed());
            assert!(!tokio::spawn(async { history_allowed() }).await.unwrap());
        })
        .await;
        assert!(!history_allowed());
    }
}
