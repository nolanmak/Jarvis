//! Account identity for owner-created SocialAPI DM approval cards.
use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_card(
    store: &Store,
    platform: &str,
    kind: &str,
    message_id: &str,
    thread_id: &str,
    account: Option<&str>,
    counterparty: &str,
    subject: &str,
    body: &str,
) -> Result<augmentagent_store::Email> {
    let mut email = social_card_row(
        platform,
        kind,
        message_id,
        thread_id,
        account,
        counterparty,
        subject,
        body,
    );
    if platform == "socialapi" && kind == "dm" && message_id.starts_with("compose:socialapi:dm:") {
        email.account_entity_id = Some(conversation_account(store, thread_id, account)?);
        // Store::upsert_email deliberately preserves account identity. A
        // reused legacy NULL-account row cannot be corrected by a later post;
        // each new approval therefore needs its own source row.
        email.message_id = format!("{message_id}:{}", uuid::Uuid::new_v4());
    }
    Ok(email)
}

/// #2331 — the cards a follow-up `socialapi dm --post` replaces: pending,
/// unscheduled compose cards (the synthetic
/// `compose:socialapi:dm:<conversation>` identity, with or without the
/// per-card suffix) for the action's own conversation. Never the triage card
/// for an inbound message, and never a card already armed with a send time.
/// Selection and retirement share the predicate so they cannot disagree on
/// which cards are in scope.
macro_rules! compose_cards {
    () => {
        "FROM actions a JOIN emails e ON e.messageId = a.messageId
            WHERE e.platform = 'socialapi' AND e.kind = 'dm'
              AND (e.messageId = 'compose:socialapi:dm:' || a.threadId
                OR substr(e.messageId, 1, length('compose:socialapi:dm:' || a.threadId || ':'))
                   = 'compose:socialapi:dm:' || a.threadId || ':')"
    };
}
const COMPOSE_CARD_SCOPE: &str =
    concat!(compose_cards!(), " AND a.status = 'pending' AND a.scheduledAtMs IS NULL");
/// The reason a follow-up compose stamps on the card it replaced.
const FOLLOW_UP: &str = "superseded by follow-up compose";

/// #2331 — the in-scope cards for `conversation`, oldest first.
///
/// #2350 — cards composed inside one millisecond share `createdAt`, so the
/// tie-break is insertion order (`rowid`); the action id is a random UUID and
/// ordering by it shuffled them.
pub(super) fn pending_compose_cards(store: &Store, conversation: &str) -> Result<Vec<String>> {
    Ok(store.with_conn(|conn| {
        let mut statement = conn.prepare(&format!(
            "SELECT a.id {COMPOSE_CARD_SCOPE} AND a.threadId = ?1
            ORDER BY a.createdAt, a.rowid"
        ))?;
        let rows = statement.query_map([conversation], |row| row.get(0))?;
        rows.collect()
    })?)
}

/// #2350 — `prior` is read before the replacement card is posted, so a card
/// can leave scope in that window: a `--send-at` proposal leaves it pending
/// with a send time set, which `Store::mark_pending_superseded_by_ids` (status
/// only) would still retire.
fn still_in_compose_scope(store: &Store, id: &str) -> Result<bool> {
    Ok(store.with_conn(|conn| {
        conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 {COMPOSE_CARD_SCOPE} AND a.id = ?1)"),
            [id],
            |row| row.get(0),
        )
    })?)
}

/// #2331 — retire the cards a follow-up compose replaced. Returns the ids
/// actually retired: a card the owner approved or skipped in the meantime is
/// left alone.
pub(super) fn supersede_prior_compose_cards(store: &Store, prior: &[String]) -> Vec<String> {
    prior
        .iter()
        .filter(|id| match still_in_compose_scope(store, id) {
            Ok(in_scope) => in_scope,
            Err(e) => {
                tracing::warn!(action_id = %id, "follow-up social DM: failed to confirm scope: {e}");
                false
            }
        })
        .filter(|id| {
            match store.mark_pending_superseded_by_ids(
                std::slice::from_ref(*id),
                FOLLOW_UP,
            ) {
                Ok(n) => n == 1,
                Err(e) => {
                    tracing::warn!(action_id = %id, "follow-up social DM: failed to supersede: {e}");
                    false
                }
            }
        })
        .cloned()
        .collect()
}

/// #2538 — reopen a card a follow-up compose replaced, so a click on it
/// reaches a live action instead of "a newer version replaced this draft"
/// with no newer card to act on.
///
/// `take_over` (Revise) makes the clicked card the conversation's one live
/// card, retiring any newer pending one: the owner is editing the draft they
/// are looking at, and the revised card is reposted. Without it (Send) the
/// card is reopened only when the conversation has no live card, so a Send
/// never ships stale text while a newer draft is waiting.
///
/// Never reopens a conversation something has since settled: a newer card
/// sent, skipped, in flight, armed with a send time or retired for another
/// reason, or an owner DM after the card was drafted.
pub(super) fn reopen_replaced_card(store: &Store, id: &str, take_over: bool) -> Result<bool> {
    use augmentagent_store::rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
    Ok(store.with_conn(|conn| {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let card: Option<(String, i64, i64)> = tx
            .query_row(
                concat!(
                    "SELECT a.threadId, a.createdAt, a.rowid ",
                    compose_cards!(),
                    " AND a.id = ?1 AND a.status = 'superseded' AND a.errorMessage = ?2"
                ),
                params![id, FOLLOW_UP],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((thread, created, rowid)) = card else {
            return Ok(false);
        };
        let settled: bool = tx.query_row(
            concat!(
                "SELECT EXISTS(SELECT 1 ",
                compose_cards!(),
                " AND a.threadId = ?1 AND a.id != ?2 AND NOT (a.status = 'pending' AND a.scheduledAtMs IS NULL)
                  AND (a.status IN ('sending', 'scheduled', 'pending')
                    OR ((a.createdAt, a.rowid) > (?3, ?4)
                      AND NOT (a.status = 'superseded' AND a.errorMessage = ?5)
                      AND NOT (a.status = 'error' AND a.errorMessage LIKE 'approval card send failed%'))))
                OR EXISTS(SELECT 1 FROM owner_messages
                  WHERE platform = 'socialapi' AND conversation_id = ?1 AND sent_at_ms > ?3)"
            ),
            params![thread, id, created, rowid, FOLLOW_UP],
            |r| r.get(0),
        )?;
        let live: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 {COMPOSE_CARD_SCOPE} AND a.threadId = ?1)"),
            [&thread],
            |r| r.get(0),
        )?;
        if settled || (live && !take_over) {
            return Ok(false);
        }
        let now = chrono::Utc::now().timestamp_millis();
        tx.execute(
            &format!(
                "UPDATE actions SET status = 'superseded', errorMessage = ?3, updatedAt = ?4
                WHERE id IN (SELECT a.id {COMPOSE_CARD_SCOPE} AND a.threadId = ?1 AND a.id != ?2)"
            ),
            params![thread, id, FOLLOW_UP, now],
        )?;
        let reopened = tx.execute(
            "UPDATE actions SET status = 'pending', errorMessage = NULL, updatedAt = ?2
            WHERE id = ?1 AND status = 'superseded'",
            params![id, now],
        )?;
        tx.commit()?;
        Ok(reopened == 1)
    })?)
}

fn conversation_account(store: &Store, thread: &str, requested: Option<&str>) -> Result<String> {
    let known: Vec<String> = store.with_conn(|conn| {
        let mut statement = conn.prepare(
            "SELECT DISTINCT accountEntityId FROM emails
            WHERE platform='socialapi' AND kind='dm' AND threadId=?1
              AND messageId NOT LIKE 'compose:%' AND accountEntityId IS NOT NULL
              AND trim(accountEntityId) NOT IN ('','socialapi') ORDER BY accountEntityId LIMIT 2",
        )?;
        let rows = statement.query_map([thread], |row| row.get(0))?;
        rows.collect()
    })?;
    if let Some(account) = requested {
        let account = account.trim();
        anyhow::ensure!(
            !account.is_empty() && account != "socialapi",
            "--account-id must identify a SocialAPI account"
        );
        // With multiple accounts, check the requested exact scope directly;
        // the bounded inference query must never discard an explicit match.
        let matches: bool = store.with_conn(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM emails
            WHERE platform='socialapi' AND kind='dm' AND threadId=?1 AND accountEntityId=?2
              AND messageId NOT LIKE 'compose:%')",
                augmentagent_store::rusqlite::params![thread, account],
                |row| row.get(0),
            )
        })?;
        anyhow::ensure!(
            known.is_empty() || matches,
            "--account-id does not match the stored SocialAPI conversation"
        );
        return Ok(account.to_owned());
    }
    anyhow::ensure!(
        known.len() == 1,
        "SocialAPI conversation account is unknown or ambiguous; pass --account-id"
    );
    Ok(known[0].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CONVERSATION: &str = "synthetic-conversation";
    const COMPOSE: &str = "compose:socialapi:dm:synthetic-conversation";
    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().canonicalize().unwrap().join("db")).unwrap();
        let email = social_card_row(
            "socialapi",
            "dm",
            "synthetic-trigger",
            CONVERSATION,
            Some("synthetic-account"),
            "Synthetic peer",
            "Synthetic DM",
            "Synthetic context",
        );
        store.upsert_email(&email).unwrap();
        (dir, store)
    }
    fn card(store: &Store, account: Option<&str>) -> augmentagent_store::Email {
        prepare_card(
            store,
            "socialapi",
            "dm",
            COMPOSE,
            CONVERSATION,
            account,
            "Synthetic peer",
            "Synthetic DM",
            "Synthetic context",
        )
        .unwrap()
    }
    fn action(store: &Store, email: &augmentagent_store::Email) -> String {
        store.upsert_email(email).unwrap();
        store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some("Synthetic draft"),
                ActionStatus::Pending,
            )
            .unwrap()
    }
    /// Pins an action's id and creation tick so ordering assertions do not
    /// depend on the random id or on the clock advancing between cards.
    fn pin(store: &Store, id: &str, pinned: &str) -> String {
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE actions SET id = ?1, createdAt = 1000 WHERE id = ?2",
                    augmentagent_store::rusqlite::params![pinned, id],
                )
            })
            .unwrap();
        pinned.to_owned()
    }
    #[test]
    fn socialapi_compose_missing_account_can_attach_same_conversation_receipt() {
        let (_dir, store) = fixture();
        let receipt = store
            .save_draft_context_receipt("synthetic-trigger", &serde_json::json!({"version":1}))
            .unwrap();
        let email = card(&store, None);
        let id = action(&store, &email);
        assert!(store.attach_draft_context_receipt(&receipt,&id).unwrap(),
            "an omitted account discarded known conversation identity and rejected the source receipt");
    }
    #[test]
    fn socialapi_compose_explicit_account_survives_legacy_unscoped_row() {
        let (_dir, store) = fixture();
        let legacy = social_card_row(
            "socialapi",
            "dm",
            COMPOSE,
            CONVERSATION,
            None,
            "Synthetic peer",
            "Synthetic DM",
            "Synthetic context",
        );
        let legacy_action = action(&store, &legacy);
        let corrected = card(&store, Some("synthetic-account"));
        let id = action(&store, &corrected);
        let current = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(
            current.email.account_entity_id.as_deref(),
            Some("synthetic-account"),
            "reused message identity retained NULL despite an explicit correct account"
        );
        let receipt = store
            .save_draft_context_receipt("synthetic-trigger", &serde_json::json!({"version":1}))
            .unwrap();
        assert!(store.attach_draft_context_receipt(&receipt, &id).unwrap());
        assert_eq!(
            store
                .get_action_with_email(&legacy_action)
                .unwrap()
                .unwrap()
                .email
                .account_entity_id,
            None,
            "new card preparation must not rewrite an earlier action's identity"
        );
    }
    #[test]
    fn socialapi_compose_unknown_ambiguous_and_conflicting_accounts_are_refused() {
        let (_dir, store) = fixture();
        assert!(prepare_card(
            &store,
            "socialapi",
            "dm",
            COMPOSE,
            CONVERSATION,
            Some("foreign-account"),
            "Peer",
            "DM",
            "Context"
        )
        .is_err());
        assert!(prepare_card(
            &store,
            "socialapi",
            "dm",
            COMPOSE,
            "unknown-conversation",
            None,
            "Peer",
            "DM",
            "Context"
        )
        .is_err());
        let other = social_card_row(
            "socialapi",
            "dm",
            "synthetic-other-source",
            CONVERSATION,
            Some("synthetic-other-account"),
            "Peer",
            "DM",
            "Context",
        );
        store.upsert_email(&other).unwrap();
        assert!(prepare_card(
            &store,
            "socialapi",
            "dm",
            COMPOSE,
            CONVERSATION,
            None,
            "Peer",
            "DM",
            "Context"
        )
        .is_err());
        assert_eq!(
            card(&store, Some("synthetic-account"))
                .account_entity_id
                .as_deref(),
            Some("synthetic-account")
        );
        assert!(store.pending_actions().unwrap().is_empty());
    }
    #[test]
    fn socialapi_compose_receipts_keep_exact_account_thread_and_platform_boundaries() {
        let (_dir, store) = fixture();
        let target = card(&store, None);
        let id = action(&store, &target);
        for (source, platform, account, thread) in [
            (
                "foreign-account",
                "socialapi",
                "foreign-account",
                CONVERSATION,
            ),
            (
                "foreign-thread",
                "socialapi",
                "synthetic-account",
                "foreign-conversation",
            ),
            (
                "foreign-platform",
                "instagram",
                "synthetic-account",
                CONVERSATION,
            ),
        ] {
            let row = social_card_row(
                platform,
                "dm",
                source,
                thread,
                Some(account),
                "Peer",
                "DM",
                "Context",
            );
            store.upsert_email(&row).unwrap();
            let receipt = store
                .save_draft_context_receipt(source, &serde_json::json!({"version":1}))
                .unwrap();
            assert!(!store.attach_draft_context_receipt(&receipt, &id).unwrap());
        }
        let receipt = store
            .save_draft_context_receipt("synthetic-trigger", &serde_json::json!({"version":1}))
            .unwrap();
        assert!(store.attach_draft_context_receipt(&receipt, &id).unwrap());
        let second = card(&store, Some("synthetic-account"));
        let second_id = action(&store, &second);
        assert!(!store
            .attach_draft_context_receipt(&receipt, &second_id)
            .unwrap());
        assert_eq!(
            store
                .get_action_with_email(&id)
                .unwrap()
                .unwrap()
                .action
                .status,
            "pending"
        );
    }

    #[test]
    fn a_follow_up_compose_retires_only_earlier_pending_compose_cards_for_that_conversation() {
        let (_dir, store) = fixture();
        // The inbound triage card for the same conversation is not a compose card.
        let inbound = social_card_row("socialapi", "dm", "synthetic-trigger", CONVERSATION,
            Some("synthetic-account"), "Synthetic peer", "Synthetic DM", "Synthetic context");
        let triage = action(&store, &inbound);
        // A pre-suffix legacy compose card and two current ones. All three
        // share one creation tick and carry descending ids, so the expected
        // oldest-first order can only come from insertion order.
        let legacy = social_card_row("socialapi", "dm", COMPOSE, CONVERSATION,
            Some("synthetic-account"), "Synthetic peer", "Synthetic DM", "Synthetic context");
        let legacy = pin(&store, &action(&store, &legacy), "c0000000-0000-4000-8000-000000000001");
        let first = pin(&store, &action(&store, &card(&store, None)), "b0000000-0000-4000-8000-000000000002");
        let second = pin(&store, &action(&store, &card(&store, None)), "a0000000-0000-4000-8000-000000000003");
        // A compose card for another conversation, and one already scheduled.
        let other = social_card_row("socialapi", "dm", "compose:socialapi:dm:synthetic-conversation-2:x",
            "synthetic-conversation-2", Some("synthetic-account"), "Other", "DM", "Context");
        let other = action(&store, &other);
        let scheduled = action(&store, &card(&store, None));
        store.with_conn(|c| c.execute("UPDATE actions SET scheduledAtMs = 1 WHERE id = ?1", [&scheduled])).unwrap();

        let prior = pending_compose_cards(&store, CONVERSATION).unwrap();
        assert_eq!(prior, vec![legacy.clone(), first.clone(), second.clone()]);

        // The owner approved one in the meantime: it is not retired.
        store.update_action_status(&first, ActionStatus::Sent, None, None).unwrap();
        let retired = supersede_prior_compose_cards(&store, &prior);
        assert_eq!(retired, vec![legacy.clone(), second.clone()]);

        let status = |id: &str| store.get_action_with_email(id).unwrap().unwrap().action.status;
        assert_eq!(status(&legacy), "superseded");
        assert_eq!(status(&second), "superseded");
        assert_eq!(status(&first), "sent");
        assert_eq!(status(&triage), "pending");
        assert_eq!(status(&other), "pending");
        assert_eq!(status(&scheduled), "pending");
        assert!(pending_compose_cards(&store, CONVERSATION).unwrap().is_empty());
    }

    /// A pending card on `conversation`, seeded straight into the store so the
    /// scope checks can be exercised without going through account inference.
    fn seed_card(store: &Store, message_id: &str, conversation: &str) -> String {
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, firstSeenAt, platform, kind)
                    VALUES (?1, ?2, 'Synthetic peer', 'Synthetic DM', 'Synthetic context', 0, 'socialapi', 'dm')",
                    augmentagent_store::rusqlite::params![message_id, conversation],
                )
            })
            .unwrap();
        store
            .log_action(
                message_id,
                Some(conversation),
                "Synthetic peer",
                "Synthetic DM",
                Some("Synthetic context"),
                Some("Synthetic draft"),
                ActionStatus::Pending,
            )
            .unwrap()
    }

    #[test]
    fn retirement_re_checks_scope_for_cards_that_left_it_after_selection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().canonicalize().unwrap().join("db")).unwrap();
        let triage = seed_card(&store, "synthetic-trigger", CONVERSATION);
        let armed = seed_card(&store, &format!("{COMPOSE}:armed"), CONVERSATION);
        let live = seed_card(&store, &format!("{COMPOSE}:live"), CONVERSATION);

        let mut prior = pending_compose_cards(&store, CONVERSATION).unwrap();
        assert_eq!(prior, vec![armed.clone(), live.clone()]);
        // While the replacement card was being posted the owner proposed a
        // send time on one of the prior cards: it stays pending, but is no
        // longer a card a follow-up compose may retire.
        store.set_action_scheduled_at(&armed, Some(1)).unwrap();
        // An inbound triage card is out of scope however it reaches here.
        prior.push(triage.clone());

        let retired = supersede_prior_compose_cards(&store, &prior);
        assert_eq!(retired, vec![live.clone()]);
        let status = |id: &str| store.get_action_with_email(id).unwrap().unwrap().action.status;
        assert_eq!(status(&live), "superseded");
        assert_eq!(status(&armed), "pending");
        assert_eq!(status(&triage), "pending");
    }

    /// #2538 — one `socialapi dm --post`: the new card goes up, then the
    /// earlier pending compose cards are retired, as `run_socialapi_dm` does.
    fn post(store: &Store) -> String {
        let prior = pending_compose_cards(store, CONVERSATION).unwrap();
        let id = action(store, &card(store, None));
        supersede_prior_compose_cards(store, &prior);
        id
    }

    struct Redraft;
    #[async_trait::async_trait]
    impl augmentagent_channel_core::Reasoner for Redraft {
        async fn call(
            &self,
            opts: &augmentagent_channel_core::reasoner::ReasonerOpts,
            _prompt: &str,
        ) -> anyhow::Result<String> {
            if opts.system_prompt.contains("You select evidence") {
                return Ok(r#"{"action":"ready","facts":[],"gaps":[]}"#.into());
            }
            Ok("Revised synthetic draft".into())
        }
    }

    async fn approver(
        dir: &tempfile::TempDir,
        store: &Arc<Store>,
        sends: u64,
    ) -> (ReplyApprover, wiremock::MockServer) {
        use augmentagent_channel_core::{cooldown::CooldownLatch, providers::ProviderKind};
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path(format!("/inbox/conversations/{CONVERSATION}/messages")))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "synthetic-sent"})),
            )
            .expect(sends)
            .mount(&server)
            .await;
        let mut approver = crate::test_support::approver_with_store(Arc::clone(store));
        approver.reasoner = Arc::new(FallbackReasoner::for_tests(
            vec![(ProviderKind::Claude, Arc::new(Redraft))],
            CooldownLatch::at(dir.path().join("cooldowns.json")),
        ));
        approver.socialapi = Some(Arc::new(
            augmentagent_channel_socialapi::SocialApiClient::with_base_url(
                augmentagent_channel_socialapi::SocialApiAuth::new("synthetic-key"),
                server.uri(),
            ),
        ));
        (approver, server)
    }

    fn status(store: &Store, id: &str) -> String {
        store.get_action_with_email(id).unwrap().unwrap().action.status
    }

    /// #2538 — post, post again, then Revise on the card the owner still
    /// sees: the revise must land on a live action (not "a newer version
    /// replaced this draft"), leave exactly one live card, and Send on it
    /// must execute.
    #[tokio::test]
    async fn revise_on_a_replaced_card_leaves_one_live_card_that_sends() {
        let (dir, store) = fixture();
        let store = Arc::new(store);
        let first = post(&store);
        let second = post(&store);
        assert_eq!(pending_compose_cards(&store, CONVERSATION).unwrap(), vec![second.clone()]);
        let (approver, _server) = approver(&dir, &store, 1).await;

        let revised = approver.revise(&first, "make it warmer").await;
        assert!(matches!(revised, ApprovalActionOutcome::Revised { .. }), "{revised:?}");
        assert_eq!(
            pending_compose_cards(&store, CONVERSATION).unwrap(),
            vec![first.clone()],
            "Revise must leave exactly one live card: the one it reposts"
        );
        assert_eq!(status(&store, &second), "superseded");

        let sent = approver.approve(&first).await;
        assert!(matches!(sent, ApprovalActionOutcome::Approved), "{sent:?}");
        assert_eq!(status(&store, &first), "sent");
    }

    /// #2538 — the stranded state from the issue: every compose card for the
    /// conversation is superseded. Send on one must still go somewhere live.
    #[tokio::test]
    async fn send_on_a_stranded_conversation_reopens_and_sends_the_clicked_card() {
        let (dir, store) = fixture();
        let store = Arc::new(store);
        let first = post(&store);
        let second = post(&store);
        store
            .mark_pending_superseded_by_ids(&[second.clone()], "superseded by follow-up compose")
            .unwrap();
        assert!(pending_compose_cards(&store, CONVERSATION).unwrap().is_empty());
        let (approver, _server) = approver(&dir, &store, 1).await;

        let sent = approver.approve(&first).await;
        assert!(matches!(sent, ApprovalActionOutcome::Approved), "{sent:?}");
        assert_eq!(status(&store, &first), "sent");
        assert_eq!(status(&store, &second), "superseded");
    }

    /// #2538 — while a live card exists, Send on a stale one never ships the
    /// stale text: it points at the live card, which stays actionable. And a
    /// card the owner already settled (sent) is never reopened behind it.
    #[tokio::test]
    async fn stale_send_points_at_the_live_card_and_settled_threads_stay_closed() {
        let (dir, store) = fixture();
        let store = Arc::new(store);
        let first = post(&store);
        let second = post(&store);
        let (approver, _server) = approver(&dir, &store, 0).await;

        let stale = approver.approve(&first).await;
        assert!(
            matches!(&stale, ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "superseded"),
            "{stale:?}"
        );
        assert_eq!(pending_compose_cards(&store, CONVERSATION).unwrap(), vec![second.clone()]);

        store.update_action_status(&second, ActionStatus::Sent, None, None).unwrap();
        for outcome in [approver.approve(&first).await, approver.revise(&first, "again").await] {
            assert!(matches!(outcome, ApprovalActionOutcome::AlreadyResolved { .. }), "{outcome:?}");
        }
        assert_eq!(status(&store, &first), "superseded");
    }

    #[test]
    fn a_conversation_id_with_like_wildcards_matches_only_itself() {
        let (_dir, store) = fixture();
        let row = |conv: &str| social_card_row("socialapi", "dm", &format!("compose:socialapi:dm:{conv}:x"),
            conv, Some("synthetic-account"), "Peer", "DM", "Context");
        let wild = action(&store, &row("conv_%"));
        action(&store, &row("conv_ab"));
        assert_eq!(pending_compose_cards(&store, "conv_%").unwrap(), vec![wild]);
    }
}
