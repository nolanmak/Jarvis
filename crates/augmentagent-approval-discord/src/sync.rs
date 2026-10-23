//! #1289 — cross-surface approval synchronization.
//!
//! One approval can have cards on several surfaces (Discord, Slack, and later
//! WhatsApp). The decision itself is already exactly-once: every handler verb
//! goes through the store's compare-and-swap transitions, so two surfaces
//! racing on one action get one winner and the loser an `AlreadyResolved`.
//! What was missing is the *cards*: a decision taken on one surface left the
//! other surface's card live until someone clicked it.
//!
//! * [`ApprovalCardSurface`] — a surface that can redraw every card it holds
//!   for an action from the store's current truth (Slack edits its cards in
//!   place through durable pointers; Discord edits the card it finds by its
//!   button IDs).
//! * [`CardSurfaces`] — the daemon's registry of those surfaces (weak
//!   references; the surfaces are owned by `serve`).
//! * [`SyncingActionHandler`] — wraps the real [`ApprovalActionHandler`] for
//!   one surface: after every verb it asks every *other* surface to redraw.
//!   The originating surface updates its own card itself, as it always did.
//! * [`MultiSurfaceBroker`] — posts each new card to every configured
//!   surface (the routing `serve` chooses: Discord, Slack or both).
//!
//! A surface that is not running (a WhatsApp card surface is not wired in
//! `serve` yet) simply is not registered; adding one is registering it here.

use std::sync::{Arc, RwLock, Weak};

use async_trait::async_trait;
use augmentagent_store::Email;
use tracing::warn;

use crate::{ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalError};

/// A surface whose approval cards can be redrawn from the store.
#[async_trait]
pub trait ApprovalCardSurface: Send + Sync {
    /// Stable surface name (`discord`, `slack`, `whatsapp`).
    fn surface_name(&self) -> &'static str;

    /// Redraw every card this surface holds for `action_id` so it shows the
    /// action's current state. `origin` is the surface where the decision
    /// was taken. Best effort: failures are logged, never returned, because
    /// the decision already happened.
    async fn redraw_cards(&self, action_id: &str, origin: &str);
}

/// The daemon's card surfaces. Cheap to clone; holds weak references.
#[derive(Clone, Default)]
pub struct CardSurfaces {
    inner: Arc<RwLock<Vec<Weak<dyn ApprovalCardSurface>>>>,
}

impl CardSurfaces {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a surface. It stays registered while something else owns it.
    pub fn register(&self, surface: &Arc<dyn ApprovalCardSurface>) {
        let mut list = self.inner.write().expect("card surfaces lock poisoned");
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(surface));
    }

    fn live(&self) -> Vec<Arc<dyn ApprovalCardSurface>> {
        self.inner
            .read()
            .expect("card surfaces lock poisoned")
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }

    /// Names of the live registered surfaces, in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.live().iter().map(|s| s.surface_name()).collect()
    }

    /// Ask every live surface except `origin` to redraw `action_id`.
    pub async fn redraw_except(&self, origin: &str, action_id: &str) {
        for surface in self.live() {
            if surface.surface_name() != origin {
                surface.redraw_cards(action_id, origin).await;
            }
        }
    }
}

tokio::task_local! {
    static DECIDING_SURFACE: &'static str;
}

/// #1290 — run `f` (a handler call) as a decision taken on `surface`, so the
/// handler can record where it came from ([`deciding_surface`]).
pub async fn deciding<F: std::future::Future>(surface: &'static str, f: F) -> F::Output {
    DECIDING_SURFACE.scope(surface, f).await
}

/// The surface the current decision was taken on (`discord`, `slack`), when
/// the caller said; `None` outside [`deciding`].
pub fn deciding_surface() -> Option<&'static str> {
    DECIDING_SURFACE.try_with(|s| *s).ok()
}

/// The real handler, plus a redraw of every other surface after each verb.
pub struct SyncingActionHandler {
    origin: &'static str,
    inner: Arc<dyn ApprovalActionHandler>,
    surfaces: CardSurfaces,
}

impl SyncingActionHandler {
    pub fn new(
        origin: &'static str,
        inner: Arc<dyn ApprovalActionHandler>,
        surfaces: CardSurfaces,
    ) -> Self {
        Self {
            origin,
            inner,
            surfaces,
        }
    }

    /// Every verb may have moved the action (or found it already moved by
    /// a path that does not sync, such as the reconcile sweep), so every
    /// outcome redraws the other surfaces; a redraw of an unchanged card is
    /// harmless.
    async fn synced(
        &self,
        action_id: &str,
        outcome: ApprovalActionOutcome,
    ) -> ApprovalActionOutcome {
        self.surfaces.redraw_except(self.origin, action_id).await;
        outcome
    }
}

#[async_trait]
impl ApprovalActionHandler for SyncingActionHandler {
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.approve(action_id)).await;
        self.synced(action_id, out).await
    }
    async fn revise(&self, action_id: &str, feedback: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.revise(action_id, feedback)).await;
        self.synced(action_id, out).await
    }
    async fn skip(&self, action_id: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.skip(action_id)).await;
        self.synced(action_id, out).await
    }
    async fn is_resolved(&self, action_id: &str) -> bool {
        self.inner.is_resolved(action_id).await
    }
    async fn schedule(&self, action_id: &str, at_ms: i64) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.schedule(action_id, at_ms)).await;
        self.synced(action_id, out).await
    }
    async fn reschedule(&self, action_id: &str, at_ms: i64) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.reschedule(action_id, at_ms)).await;
        self.synced(action_id, out).await
    }
    async fn send_now(&self, action_id: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.send_now(action_id)).await;
        self.synced(action_id, out).await
    }
    async fn cancel_schedule(&self, action_id: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.cancel_schedule(action_id)).await;
        self.synced(action_id, out).await
    }
    async fn back_to_queue(&self, action_id: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.back_to_queue(action_id)).await;
        self.synced(action_id, out).await
    }
    async fn recompose(&self, action_id: &str) -> ApprovalActionOutcome {
        let out = deciding(self.origin, self.inner.recompose(action_id)).await;
        self.synced(action_id, out).await
    }
    async fn is_schedule_live(&self, action_id: &str) -> bool {
        self.inner.is_schedule_live(action_id).await
    }
}

/// Posts every card and notice to each configured surface.
pub struct MultiSurfaceBroker {
    surfaces: Vec<(&'static str, Arc<dyn ApprovalBroker>)>,
}

impl MultiSurfaceBroker {
    pub fn new(surfaces: Vec<(&'static str, Arc<dyn ApprovalBroker>)>) -> Self {
        Self { surfaces }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.surfaces.iter().map(|(n, _)| *n).collect()
    }
}

/// `Ok` when at least one surface took it; otherwise the first error. A
/// surface that fails is logged so one outage never hides the card from the
/// others.
// `ApprovalError` is the broker trait's error type (its serenity variant is
// large); boxing it here would diverge from every other broker method.
#[allow(clippy::result_large_err)]
fn any_ok<T>(
    what: &str,
    results: Vec<(&'static str, Result<T, ApprovalError>)>,
) -> Result<Vec<T>, ApprovalError> {
    let mut ok = Vec::new();
    let mut first_err = None;
    for (name, result) in results {
        match result {
            Ok(v) => ok.push(v),
            Err(e) => {
                warn!(
                    surface = name,
                    "approval {what} failed on this surface: {e}"
                );
                first_err.get_or_insert(e);
            }
        }
    }
    match (ok.is_empty(), first_err) {
        (true, Some(e)) => Err(e),
        _ => Ok(ok),
    }
}

#[async_trait]
impl ApprovalBroker for MultiSurfaceBroker {
    async fn post_owner_alert(&self,notice:&augmentagent_store::alert_schedule::AlertNotice)
        ->Result<String,ApprovalError> {
        let Some((_,discord))=self.surfaces.iter().find(|(name,_)|*name=="discord") else {
            return Err(ApprovalError::Discord("Discord owner-alert surface is unavailable".into()));
        };
        discord.post_owner_alert(notice).await
    }
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        let mut results = Vec::new();
        for (name, broker) in &self.surfaces {
            results.push((*name, broker.post_approval(action_id, email, draft).await));
        }
        any_ok("card", results).map(|_| ())
    }

    async fn post_flag_notice(&self, email: &Email, reason: &str) -> Result<(), ApprovalError> {
        let mut results = Vec::new();
        for (name, broker) in &self.surfaces {
            results.push((*name, broker.post_flag_notice(email, reason).await));
        }
        any_ok("flag notice", results).map(|_| ())
    }

    async fn post_digest(&self, title: &str, body: &str) -> Result<(), ApprovalError> {
        let mut results = Vec::new();
        for (name, broker) in &self.surfaces {
            results.push((*name, broker.post_digest(title, body).await));
        }
        any_ok("digest", results).map(|_| ())
    }

    async fn post_scheduled_notice(
        &self,
        action_id: &str,
        email: &Email,
        sends_at_local: &str,
        sends_at_ms: i64,
        to_display: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        let mut results = Vec::new();
        for (name, broker) in &self.surfaces {
            results.push((
                *name,
                broker
                    .post_scheduled_notice(
                        action_id,
                        email,
                        sends_at_local,
                        sends_at_ms,
                        to_display,
                    )
                    .await,
            ));
        }
        Ok(any_ok("scheduled notice", results)?
            .into_iter()
            .flatten()
            .next())
    }

    async fn post_approval_card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        redraft_count: u32,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        let mut results = Vec::new();
        for (name, broker) in &self.surfaces {
            results.push((
                *name,
                broker
                    .post_approval_card(action_id, email, draft, redraft_count)
                    .await,
            ));
        }
        Ok(any_ok("card", results)?.into_iter().flatten().next())
    }

    /// The message ids are the ones a surface returned from a post (only
    /// Discord returns any today), so every surface is asked and the ones
    /// that do not own the message ignore it.
    async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), ApprovalError> {
        let mut results = Vec::new();
        for (name, broker) in &self.surfaces {
            results.push((*name, broker.delete_message(channel_id, message_id).await));
        }
        any_ok("delete", results).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn email() -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "m-1".into(),
            thread_id: None,
            from: "contact@example.com".into(),
            subject: "Hello".into(),
            body: "hi".into(),
            date: String::new(),
            account_entity_id: None,
            platform: "slack".into(),
            kind: "dm".into(),
        }
    }

    #[derive(Default)]
    struct Recorder {
        name: &'static str,
        redraws: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl ApprovalCardSurface for Recorder {
        fn surface_name(&self) -> &'static str {
            self.name
        }
        async fn redraw_cards(&self, action_id: &str, origin: &str) {
            self.redraws
                .lock()
                .unwrap()
                .push((action_id.into(), origin.into()));
        }
    }

    fn recorder(name: &'static str) -> Arc<Recorder> {
        Arc::new(Recorder {
            name,
            ..Default::default()
        })
    }

    struct Fixed(ApprovalActionOutcome);

    #[async_trait]
    impl ApprovalActionHandler for Fixed {
        async fn approve(&self, _: &str) -> ApprovalActionOutcome {
            self.0.clone()
        }
        async fn revise(&self, _: &str, _: &str) -> ApprovalActionOutcome {
            self.0.clone()
        }
        async fn skip(&self, _: &str) -> ApprovalActionOutcome {
            self.0.clone()
        }
        async fn is_resolved(&self, _: &str) -> bool {
            true
        }
    }

    /// #1290 — the handler can tell which surface a decision came from, so
    /// the store records `status_source = slack` for a Slack click instead
    /// of the old hard-coded `discord`.
    struct SeesSurface(std::sync::Mutex<Vec<Option<&'static str>>>);

    #[async_trait]
    impl ApprovalActionHandler for SeesSurface {
        async fn reschedule(&self, _: &str, _: i64) -> ApprovalActionOutcome {
            self.0.lock().unwrap().push(deciding_surface());
            ApprovalActionOutcome::Scheduled {
                at_ms: 5,
                local: String::new(),
            }
        }
        async fn approve(&self, _: &str) -> ApprovalActionOutcome {
            self.0.lock().unwrap().push(deciding_surface());
            ApprovalActionOutcome::Approved
        }
        async fn revise(&self, _: &str, _: &str) -> ApprovalActionOutcome {
            self.0.lock().unwrap().push(deciding_surface());
            ApprovalActionOutcome::Approved
        }
        async fn skip(&self, _: &str) -> ApprovalActionOutcome {
            self.0.lock().unwrap().push(deciding_surface());
            ApprovalActionOutcome::Skipped
        }
        async fn is_resolved(&self, _: &str) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn the_deciding_surface_is_visible_to_the_handler() {
        let seen = Arc::new(SeesSurface(std::sync::Mutex::new(Vec::new())));
        let inner: Arc<dyn ApprovalActionHandler> = seen.clone();
        // No surface said: callers fall back to their own default.
        inner.approve("a").await;
        // The Discord bot's wrapper.
        let discord = SyncingActionHandler::new("discord", Arc::clone(&inner), CardSurfaces::new());
        discord.skip("a").await;
        // The Slack surface calls the handler directly inside its scope.
        deciding("slack", inner.revise("a", "shorter")).await;
        // #1291 — a reschedule from Discord is recorded as Discord.
        discord.reschedule("a", 5).await;
        assert_eq!(
            *seen.0.lock().unwrap(),
            vec![None, Some("discord"), Some("slack"), Some("discord")]
        );
    }

    #[tokio::test]
    async fn every_verb_redraws_the_other_surfaces_but_never_the_origin() {
        let surfaces = CardSurfaces::new();
        let slack = recorder("slack");
        let discord = recorder("discord");
        let slack_dyn: Arc<dyn ApprovalCardSurface> = slack.clone();
        let discord_dyn: Arc<dyn ApprovalCardSurface> = discord.clone();
        surfaces.register(&slack_dyn);
        surfaces.register(&discord_dyn);
        assert_eq!(surfaces.names(), vec!["slack", "discord"]);

        let handler = SyncingActionHandler::new(
            "discord",
            Arc::new(Fixed(ApprovalActionOutcome::Approved)),
            surfaces.clone(),
        );
        assert!(matches!(
            handler.approve("a1").await,
            ApprovalActionOutcome::Approved
        ));
        handler.skip("a2").await;
        handler.revise("a3", "shorter").await;
        handler.recompose("a4").await;
        // #1291 — the schedule verbs redraw too, reschedule included.
        handler.schedule("a6", 1).await;
        handler.reschedule("a7", 2).await;
        handler.send_now("a8").await;
        handler.cancel_schedule("a9").await;
        handler.back_to_queue("a10").await;
        // A read never redraws.
        assert!(handler.is_resolved("a5").await);

        assert_eq!(
            slack.redraws.lock().unwrap().clone(),
            vec![
                ("a1".to_string(), "discord".to_string()),
                ("a2".into(), "discord".into()),
                ("a3".into(), "discord".into()),
                ("a4".into(), "discord".into()),
                ("a6".into(), "discord".into()),
                ("a7".into(), "discord".into()),
                ("a8".into(), "discord".into()),
                ("a9".into(), "discord".into()),
                ("a10".into(), "discord".into()),
            ]
        );
        assert!(
            discord.redraws.lock().unwrap().is_empty(),
            "the originating surface updates its own card"
        );
    }

    #[tokio::test]
    async fn an_already_resolved_answer_still_redraws_so_stale_cards_catch_up() {
        let surfaces = CardSurfaces::new();
        let slack = recorder("slack");
        let slack_dyn: Arc<dyn ApprovalCardSurface> = slack.clone();
        surfaces.register(&slack_dyn);
        let handler = SyncingActionHandler::new(
            "discord",
            Arc::new(Fixed(ApprovalActionOutcome::AlreadyResolved {
                status: "sent".into(),
                detail: None,
            })),
            surfaces,
        );
        handler.approve("a1").await;
        assert_eq!(slack.redraws.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_dropped_surface_is_forgotten() {
        let surfaces = CardSurfaces::new();
        {
            let gone: Arc<dyn ApprovalCardSurface> = recorder("whatsapp");
            surfaces.register(&gone);
        }
        let kept: Arc<dyn ApprovalCardSurface> = recorder("slack");
        surfaces.register(&kept);
        assert_eq!(surfaces.names(), vec!["slack"]);
        surfaces.redraw_except("discord", "a1").await;
    }

    #[derive(Default)]
    struct PostRecorder {
        posted: Mutex<Vec<String>>,
        flags: Mutex<Vec<String>>,
        fail: bool,
        ids: Option<(u64, u64)>,
    }

    #[async_trait]
    impl ApprovalBroker for PostRecorder {
        async fn post_approval(
            &self,
            action_id: &str,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
            if self.fail {
                return Err(ApprovalError::Discord("down".into()));
            }
            self.posted.lock().unwrap().push(action_id.into());
            Ok(())
        }
        async fn post_flag_notice(&self, e: &Email, _: &str) -> Result<(), ApprovalError> {
            if self.fail {
                return Err(ApprovalError::Discord("down".into()));
            }
            self.flags.lock().unwrap().push(e.subject.clone());
            Ok(())
        }
        async fn post_approval_card(
            &self,
            action_id: &str,
            email: &Email,
            draft: &str,
            _: u32,
        ) -> Result<Option<(u64, u64)>, ApprovalError> {
            self.post_approval(action_id, email, draft).await?;
            Ok(self.ids)
        }
    }

    #[tokio::test]
    async fn cards_and_notices_reach_every_surface() {
        let discord = Arc::new(PostRecorder {
            ids: Some((1, 2)),
            ..Default::default()
        });
        let slack = Arc::new(PostRecorder::default());
        let broker = MultiSurfaceBroker::new(vec![
            ("discord", discord.clone() as Arc<dyn ApprovalBroker>),
            ("slack", slack.clone() as Arc<dyn ApprovalBroker>),
        ]);
        assert_eq!(broker.names(), vec!["discord", "slack"]);
        broker.post_approval("a1", &email(), "draft").await.unwrap();
        broker.post_flag_notice(&email(), "why").await.unwrap();
        assert_eq!(
            broker
                .post_approval_card("a2", &email(), "draft", 0)
                .await
                .unwrap(),
            Some((1, 2)),
            "the first surface that returns message ids wins"
        );
        assert_eq!(discord.posted.lock().unwrap().clone(), vec!["a1", "a2"]);
        assert_eq!(slack.posted.lock().unwrap().clone(), vec!["a1", "a2"]);
        assert_eq!(slack.flags.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn one_surface_failing_does_not_lose_the_card_but_all_failing_is_an_error() {
        let down = Arc::new(PostRecorder {
            fail: true,
            ..Default::default()
        });
        let up = Arc::new(PostRecorder::default());
        let broker = MultiSurfaceBroker::new(vec![
            ("discord", down.clone() as Arc<dyn ApprovalBroker>),
            ("slack", up.clone() as Arc<dyn ApprovalBroker>),
        ]);
        broker.post_approval("a1", &email(), "draft").await.unwrap();
        assert_eq!(up.posted.lock().unwrap().clone(), vec!["a1"]);

        let all_down = MultiSurfaceBroker::new(vec![("discord", down as Arc<dyn ApprovalBroker>)]);
        assert!(all_down
            .post_approval("a1", &email(), "draft")
            .await
            .is_err());
        assert!(all_down.post_flag_notice(&email(), "why").await.is_err());
    }
}
