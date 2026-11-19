//! #2284 — the cross-channel half of the stale-approval sweep's Rule 1.
//!
//! The sweep (`reconcile_stale_approvals_pass` in `main.rs`) retires a
//! pending reply card once the owner has sent anything in the card's
//! conversation after the card's inbound message, on any channel
//! ([`augmentagent_store::Store::owner_replied_since`]). This module holds the
//! pieces that keep that rule safe: composes are exempt, lookups are bounded
//! per tick (rotating so no card starves), and retired cards are redrawn on
//! every surface. It is model-free: it reads the store and nothing else.

use std::sync::atomic::{AtomicUsize, Ordering};

use augmentagent_approval_discord::CardSurfaces;
use augmentagent_store::rusqlite::{params, OptionalExtension};
use augmentagent_store::{PendingActionRow, Store};
use tracing::{info, warn};

/// Owner-reply lookups per sweep tick. Each is a handful of indexed point
/// queries; the pending queue is normally a few dozen cards.
pub(crate) const REPLY_CHECKS_PER_TICK: usize = 200;

/// Card redraws per tick. Discord finds a card by scanning recent channel
/// messages, so each redraw is an API call; anything past this is left to
/// each surface's own resolved-card poll.
const REDRAWS_PER_TICK: usize = 50;

/// Shown on a redrawn card as "decided on …".
const ORIGIN: &str = "auto-sweep";

/// `emails.kind` of an owner-started message (#1635). Never retired here.
const COMPOSE_KIND: &str = "compose";

/// Where the next over-budget tick starts in the pending list.
static NEXT_START: AtomicUsize = AtomicUsize::new(0);

/// What one sweep pass did.
#[derive(Debug, Default)]
pub(crate) struct Pass {
    /// Rows actually flipped.
    pub retired: usize,
    /// Every card the pass tried to retire, to redraw.
    pub touched: Vec<String>,
}

/// An owner-started compose is not an answer to anyone, so the owner's own
/// messages never make it stale.
pub(crate) fn is_compose(row: &PendingActionRow) -> bool {
    row.kind == COMPOSE_KIND
}

/// The time the card's inbound arrived: its `receivedAt`, else the indexed
/// time of the same message (same parser as the owner rows it is compared
/// with), else when the card was raised, which is always after the inbound.
pub(crate) fn inbound_bound_ms(store: &Store, row: &PendingActionRow) -> i64 {
    if let Some(ms) = row
        .received_at
        .as_deref()
        .map(crate::team_coverage::message_time_ms)
        .filter(|ms| *ms > 0)
    {
        return ms;
    }
    let indexed = store.with_conn(|c| {
        c.query_row(
            "SELECT ts_ms FROM message_index WHERE message_id = ?1 AND ts_fallback = 0",
            params![row.message_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()
    });
    match indexed {
        Ok(Some(ms)) if ms > 0 => ms,
        _ => row.created_at_ms,
    }
}

/// A per-tick cap on owner-reply lookups. When the pending list is longer
/// than the cap, each tick starts where the last one ran out.
pub(crate) struct Budget {
    left: usize,
    len: usize,
    start: usize,
    stopped_at: Option<usize>,
    skipped: usize,
}

impl Budget {
    pub(crate) fn new(max: usize, len: usize) -> Self {
        let start = if len > max && len > 0 {
            NEXT_START.load(Ordering::Relaxed) % len
        } else {
            0
        };
        Self { left: max, len, start, stopped_at: None, skipped: 0 }
    }

    /// `rows` in this tick's order.
    pub(crate) fn order<'a, T>(&self, rows: &'a [T]) -> Vec<&'a T> {
        let start = self.start.min(rows.len());
        rows[start..].iter().chain(rows[..start].iter()).collect()
    }

    /// Spend one lookup on the row at `pos` in [`Self::order`]; false once
    /// the budget is gone.
    pub(crate) fn take(&mut self, pos: usize) -> bool {
        if self.left > 0 {
            self.left -= 1;
            return true;
        }
        self.stopped_at.get_or_insert(pos);
        self.skipped += 1;
        false
    }

    pub(crate) fn finish(&self) {
        if let Some(pos) = self.stopped_at {
            NEXT_START.store((self.start + pos) % self.len.max(1), Ordering::Relaxed);
            warn!(
                skipped = self.skipped,
                "stale approval: owner-reply check budget spent; the rest wait for the next tick"
            );
        }
    }
}

/// Redraw every surface's card for the touched actions (Discord, Slack and the
/// owner iMessage surface), as the other supersede paths do.
pub(crate) async fn redraw(surfaces: &CardSurfaces, ids: &[String]) {
    for id in ids.iter().take(REDRAWS_PER_TICK) {
        surfaces.redraw_except(ORIGIN, id).await;
    }
    if ids.len() > REDRAWS_PER_TICK {
        info!(
            left = ids.len() - REDRAWS_PER_TICK,
            "stale approval: redraw cap reached; surfaces' own polls catch the rest"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::{ActionStatus, Email};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn store() -> (Store, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("data.db")).unwrap();
        (s, dir)
    }

    fn email(id: &str, thread: &str, platform: &str, kind: &str, date: &str) -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: id.into(),
            thread_id: Some(thread.into()),
            // A human-looking sender so Gmail cards pass the bulk-sender rule.
            from: if platform == "gmail" {
                "Pat Example <pat@labs.example.com>".into() // pii-ok: synthetic
            } else {
                format!("Pat Example <{platform}:pat-0001>")
            },
            subject: "subj".into(),
            body: "body".into(),
            date: date.into(),
            account_entity_id: Some("acc".into()),
            platform: platform.into(),
            kind: kind.into(),
        }
    }

    /// A pending reply card on `platform`; inbound at 2026-07-13T12:00:00Z.
    fn card(s: &Store, id: &str, thread: &str, platform: &str, kind: &str) -> String {
        let e = email(id, thread, platform, kind, "2026-07-13T12:00:00Z");
        s.upsert_email(&e).unwrap();
        s.log_action(id, Some(thread), &e.from, "subj", Some("body"), Some("a draft"), ActionStatus::Pending)
            .unwrap()
    }

    const INBOUND_MS: i64 = 1_783_944_000_000; // 2026-07-13T12:00:00Z

    fn owner_index_row(s: &Store, id: &str, platform: &str, conv: &str, ts_ms: i64) {
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO message_index(message_id, platform, conv_kind, conversation_id, \
                 sender_handle, from_me, ts_ms, ts_fallback, has_attachment, extractor_version) \
                 VALUES (?1, ?2, 'dm', ?3, 'me', 1, ?4, 0, 0, 2)",
                params![id, platform, conv, ts_ms],
            )
        })
        .unwrap();
    }

    fn status(s: &Store, id: &str) -> String {
        s.get_action_with_email(id).unwrap().unwrap().action.status
    }

    fn pass(s: &Store, checks: usize) -> Pass {
        let team = crate::team_coverage::Config {
            mode: crate::team_coverage::Mode::Off,
            roster: augmentagent_messages::team::TeamRoster::parse(""),
            owner: augmentagent_messages::team::OwnerIdentities::new(Vec::<String>::new()),
            aliases: Vec::new(),
        };
        crate::reconcile_stale_approvals_pass(s, &team, checks).unwrap()
    }

    /// Each channel: an owner message after the inbound retires the reply
    /// card; one before it does not.
    #[test]
    fn owner_message_after_the_inbound_retires_the_card_on_every_channel() {
        let (s, _d) = store();
        let channels = [
            ("imessage", "imessage:+15550000001"),
            ("whatsapp", "whatsapp-history:15550000002@s.whatsapp.net"),
            ("linkedin", "urn:li:msg_conversation:synthetic-0001"),
            ("slack", "D00000001"),
            ("socialapi", "00000000-0000-4000-8000-000000000001"),
            ("gmail", "1a0000000000000a"),
        ];
        let mut answered = Vec::new();
        let mut open = Vec::new();
        for (i, (platform, conv)) in channels.iter().enumerate() {
            let a = card(&s, &format!("in-a-{i}"), conv, platform, "dm");
            let o = card(&s, &format!("in-o-{i}"), &format!("{conv}-other"), platform, "dm");
            match *platform {
                "socialapi" => {
                    s.record_owner_message(platform, conv, "out-1", INBOUND_MS + 60_000, "t").unwrap();
                    s.record_owner_message(platform, &format!("{conv}-other"), "out-2", INBOUND_MS - 60_000, "t")
                        .unwrap();
                }
                "gmail" => {
                    s.record_outbound_thread_event("acc", "sent-1", Some(conv), INBOUND_MS + 60_000).unwrap();
                    s.record_outbound_thread_event("acc", "sent-2", Some(&format!("{conv}-other")), INBOUND_MS - 60_000)
                        .unwrap();
                }
                _ => {
                    owner_index_row(&s, &format!("me-a-{i}"), platform, conv, INBOUND_MS + 60_000);
                    owner_index_row(&s, &format!("me-o-{i}"), platform, &format!("{conv}-other"), INBOUND_MS - 60_000);
                }
            }
            answered.push(a);
            open.push(o);
        }
        let p = pass(&s, REPLY_CHECKS_PER_TICK);
        assert_eq!(p.retired, answered.len());
        for id in &answered {
            assert_eq!(status(&s, id), "superseded", "{id}");
            assert!(p.touched.contains(id));
        }
        for id in &open {
            assert_eq!(status(&s, id), "pending", "{id}");
        }
        let row = s.get_action_with_email(&answered[0]).unwrap().unwrap();
        assert_eq!(
            row.action.error_message.as_deref(),
            Some("superseded: you already replied on this thread")
        );
    }

    /// Composes are the owner's own words, so the owner's messages never
    /// retire them, on any channel.
    #[test]
    fn composes_are_never_retired_by_owner_messages() {
        let (s, _d) = store();
        for (platform, conv) in [("imessage", "imessage:+15550000003"), ("slack", "D00000003"), ("whatsapp", "15550000004@s.whatsapp.net")] {
            let id = card(&s, &format!("c-{platform}"), conv, platform, "compose");
            owner_index_row(&s, &format!("me-{platform}"), platform, conv, INBOUND_MS + 60_000);
            s.record_outbound_thread_event("acc", &format!("sent-{platform}"), Some(conv), INBOUND_MS + 60_000)
                .unwrap();
            pass(&s, REPLY_CHECKS_PER_TICK);
            assert_eq!(status(&s, &id), "pending", "{platform}");
        }
    }

    /// A scheduled compose is never cancelled by a manual reply; a scheduled
    /// reply still is (#1640).
    #[test]
    fn scheduled_compose_survives_a_manual_reply_and_scheduled_reply_does_not() {
        let (s, _d) = store();
        let compose = card(&s, "sc-1", "1a00000000000c01", "gmail", "compose");
        let reply = card(&s, "sr-1", "1a00000000000c02", "gmail", "dm");
        s.schedule_action(&compose, INBOUND_MS + 3_600_000, "t").unwrap();
        s.schedule_action(&reply, INBOUND_MS + 3_600_000, "t").unwrap();
        let later = chrono::Utc::now().timestamp_millis() + 60_000;
        s.record_outbound_thread_event("acc", "m-c", Some("1a00000000000c01"), later).unwrap();
        s.record_outbound_thread_event("acc", "m-r", Some("1a00000000000c02"), later).unwrap();
        let p = pass(&s, REPLY_CHECKS_PER_TICK);
        assert_eq!(status(&s, &compose), "scheduled");
        assert_eq!(status(&s, &reply), "superseded");
        assert!(p.touched.contains(&reply));
    }

    /// The bound comes from the inbound's indexed time when its
    /// `receivedAt` has a shape the sweep's parser can't read.
    #[test]
    fn unparseable_received_at_falls_back_to_the_indexed_inbound_time() {
        let (s, _d) = store();
        let e = email("in-1", "imessage:+15550000005", "imessage", "dm", "13 July, noon");
        s.upsert_email(&e).unwrap();
        let id = s
            .log_action("in-1", e.thread_id.as_deref(), &e.from, "subj", Some("b"), Some("d"), ActionStatus::Pending)
            .unwrap();
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO message_index(message_id, platform, conv_kind, conversation_id, \
                 sender_handle, from_me, ts_ms, ts_fallback, has_attachment, extractor_version) \
                 VALUES ('in-1', 'imessage', 'dm', 'imessage:+15550000005', 'x', 0, 5000, 0, 0, 2)",
                [],
            )
        })
        .unwrap();
        let row = s.pending_actions_for_reconcile().unwrap().into_iter().find(|r| r.id == id).unwrap();
        assert_eq!(inbound_bound_ms(&s, &row), 5_000);
    }

    /// Lookups are capped per tick, and the next tick picks up where the last
    /// one ran out, so every answered card is retired within a few ticks.
    #[test]
    fn sweep_is_bounded_per_tick_and_rotates() {
        let (s, _d) = store();
        let mut ids = Vec::new();
        for i in 0..5 {
            let conv = format!("imessage:+1555000010{i}");
            ids.push(card(&s, &format!("b-{i}"), &conv, "imessage", "dm"));
            owner_index_row(&s, &format!("me-b-{i}"), "imessage", &conv, INBOUND_MS + 60_000);
        }
        assert_eq!(pass(&s, 2).retired, 2);
        assert_eq!(pass(&s, 2).retired, 2);
        assert_eq!(pass(&s, 2).retired, 1);
        for id in &ids {
            assert_eq!(status(&s, id), "superseded");
        }
    }

    #[test]
    fn budget_order_rotates_only_when_over_budget() {
        let rows = [1, 2, 3];
        assert_eq!(Budget::new(5, 3).order(&rows), vec![&1, &2, &3]);
        let mut b = Budget::new(1, 3);
        let order: Vec<i32> = b.order(&rows).into_iter().copied().collect();
        assert_eq!(order.len(), 3);
        assert!(b.take(0));
        assert!(!b.take(1));
        assert!(!b.take(2));
    }

    struct Recorder(std::sync::Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl augmentagent_approval_discord::ApprovalCardSurface for Recorder {
        fn surface_name(&self) -> &'static str {
            "discord"
        }
        async fn redraw_cards(&self, action_id: &str, origin: &str) {
            self.0.lock().unwrap().push(format!("{action_id}@{origin}"));
        }
    }

    /// Retired cards are redrawn on every surface, Discord included, capped
    /// per tick.
    #[tokio::test]
    async fn redraw_reaches_every_surface_and_is_capped() {
        let surfaces = CardSurfaces::new();
        let rec = Arc::new(Recorder(Default::default()));
        let dynrec: Arc<dyn augmentagent_approval_discord::ApprovalCardSurface> = rec.clone();
        surfaces.register(&dynrec);
        redraw(&surfaces, &["a1".to_string()]).await;
        assert_eq!(*rec.0.lock().unwrap(), vec![format!("a1@{ORIGIN}")]);
        let many: Vec<String> = (0..REDRAWS_PER_TICK + 5).map(|i| format!("x{i}")).collect();
        redraw(&surfaces, &many).await;
        assert_eq!(rec.0.lock().unwrap().len(), 1 + REDRAWS_PER_TICK);
    }
}
