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

/// #2331 — pending owner-composed DM cards for `conversation`, oldest first:
/// the cards a follow-up `socialapi dm --post` replaces. Only compose cards
/// (the synthetic `compose:socialapi:dm:<conversation>` identity, with or
/// without the per-card suffix), never the triage card for an inbound
/// message, and never a card already armed with a send time.
///
/// #2350 — cards composed inside one millisecond share `createdAt`, so the
/// tie-break is insertion order (`rowid`); the action id is a random UUID
/// and would order them arbitrarily.
pub(super) fn pending_compose_cards(store: &Store, conversation: &str) -> Result<Vec<String>> {
    let base = format!("compose:socialapi:dm:{conversation}");
    let prefix = format!("{base}:");
    Ok(store.with_conn(|conn| {
        let mut statement = conn.prepare(
            "SELECT a.id FROM actions a JOIN emails e ON e.messageId = a.messageId
            WHERE e.platform = 'socialapi' AND e.kind = 'dm' AND a.threadId = ?1
              AND a.status = 'pending' AND a.scheduledAtMs IS NULL
              AND (e.messageId = ?2 OR substr(e.messageId, 1, length(?3)) = ?3)
            ORDER BY a.createdAt, a.rowid",
        )?;
        let rows = statement.query_map(
            augmentagent_store::rusqlite::params![conversation, base, prefix],
            |row| row.get(0),
        )?;
        rows.collect()
    })?)
}

/// #2331 — retire the cards a follow-up compose replaced. Returns the ids
/// actually retired: a card the owner approved or skipped in the meantime is
/// left alone.
pub(super) fn supersede_prior_compose_cards(store: &Store, prior: &[String]) -> Vec<String> {
    prior
        .iter()
        .filter(|id| {
            match store.mark_pending_superseded_by_ids(
                std::slice::from_ref(*id),
                "superseded by follow-up compose",
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
