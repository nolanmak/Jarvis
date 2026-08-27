//! #45 — `ApprovalSurface`: the one contract every approval transport
//! (Discord, Telegram, PWA/Web-Push) implements.
//!
//! The crate already had two halves of this: [`crate::ApprovalBroker`] (post a
//! card) and [`crate::ApprovalActionHandler`] (resolve a click). A *surface*
//! is just both halves wired to one transport. This trait names that pairing
//! so the daemon can hold `Vec<Arc<dyn ApprovalSurface>>` and fan a new
//! pending action out to Discord + Telegram + Web Push uniformly, and so a
//! resolution on any surface flows back through the same `resolve` path
//! (which goes through the store CAS gate from #47 — see
//! `Store::try_resolve_action` — so multi-surface fan-out can't double-send).
//!
//! Discord conforms with zero behavior change: `DiscordApprovalBroker`
//! already implements `ApprovalBroker`, and the CLI's `ReplyApprover` already
//! implements `ApprovalActionHandler`. The blanket impl below makes any
//! `(broker, handler)` pair an `ApprovalSurface` for free, so existing wiring
//! is untouched and new surfaces only implement the two halves they need.

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_store::Email;

use crate::{ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalError};

/// A complete approval transport: can post a card and resolve a decision.
#[async_trait]
pub trait ApprovalSurface: Send + Sync {
    /// Stable name of this surface (`discord` | `telegram` | `web_push`).
    /// Used as the `source` tag on the #47 cross-surface broadcast so a
    /// surface can suppress its own echo.
    fn name(&self) -> &'static str;

    /// Post a fresh approval card.
    async fn post(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError>;

    /// Resolve a decision. `verb` is `approve` | `skip` | `revise`.
    async fn resolve(
        &self,
        action_id: &str,
        verb: &str,
        feedback: Option<&str>,
    ) -> ApprovalActionOutcome;
}

/// Pair an `ApprovalBroker` with an `ApprovalActionHandler` into a named
/// `ApprovalSurface`. This is how Discord becomes a surface without any code
/// change to the broker/handler themselves.
pub struct ComposedSurface {
    pub name: &'static str,
    pub broker: Arc<dyn ApprovalBroker>,
    pub handler: Arc<dyn ApprovalActionHandler>,
}

#[async_trait]
impl ApprovalSurface for ComposedSurface {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn post(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        self.broker.post_approval(action_id, email, draft).await
    }

    async fn resolve(
        &self,
        action_id: &str,
        verb: &str,
        feedback: Option<&str>,
    ) -> ApprovalActionOutcome {
        match verb {
            "approve" => self.handler.approve(action_id).await,
            "skip" => self.handler.skip(action_id).await,
            "revise" => {
                self.handler
                    .revise(action_id, feedback.unwrap_or(""))
                    .await
            }
            other => ApprovalActionOutcome::Failed {
                message: format!("unknown verb: {other}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::QueryHandler;
    use std::sync::Mutex;

    struct StubBroker {
        posted: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl ApprovalBroker for StubBroker {
        async fn post_approval(
            &self,
            id: &str,
            _e: &Email,
            _d: &str,
        ) -> Result<(), ApprovalError> {
            self.posted.lock().unwrap().push(id.into());
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _e: &Email,
            _r: &str,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
    }

    struct StubHandler;
    #[async_trait]
    impl ApprovalActionHandler for StubHandler {
        async fn approve(&self, _id: &str) -> ApprovalActionOutcome {
            ApprovalActionOutcome::Approved
        }
        async fn revise(&self, _id: &str, _f: &str) -> ApprovalActionOutcome {
            ApprovalActionOutcome::Failed {
                message: "n/a".into(),
            }
        }
        async fn skip(&self, _id: &str) -> ApprovalActionOutcome {
            ApprovalActionOutcome::Skipped
        }
        async fn is_resolved(&self, _id: &str) -> bool {
            false
        }
    }

    // Touch QueryHandler so the import graph is exercised the same way the
    // real surfaces compose; no behavior asserted here.
    #[allow(dead_code)]
    fn _qh_is_object_safe(_q: Option<Arc<dyn QueryHandler>>) {}

    fn email() -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "m".into(),
            thread_id: None,
            from: "a@b.com".into(),
            subject: "s".into(),
            body: "b".into(),
            date: String::new(),
            account_entity_id: None,
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    #[tokio::test]
    async fn composed_surface_routes_post_and_verbs() {
        let surface = ComposedSurface {
            name: "discord",
            broker: Arc::new(StubBroker {
                posted: Mutex::new(vec![]),
            }),
            handler: Arc::new(StubHandler),
        };
        assert_eq!(surface.name(), "discord");
        surface.post("a1", &email(), "draft").await.unwrap();
        assert!(matches!(
            surface.resolve("a1", "approve", None).await,
            ApprovalActionOutcome::Approved
        ));
        assert!(matches!(
            surface.resolve("a1", "skip", None).await,
            ApprovalActionOutcome::Skipped
        ));
        assert!(matches!(
            surface.resolve("a1", "bogus", None).await,
            ApprovalActionOutcome::Failed { .. }
        ));
    }
}
