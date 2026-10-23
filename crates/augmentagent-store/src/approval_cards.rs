//! #1289 — durable approval-card pointers, one row per posted card per
//! surface, plus explicit short action references for text commands.
//!
//! An approval (an `actions` row) can be shown on several surfaces at once
//! (Slack, Discord, …) and more than once on one surface (a reminder repost).
//! Every surface that can redraw a card in place records where it put each
//! card here, so a decision taken anywhere — or a restart, or a reconcile
//! sweep — can find every card and redraw it from the action's current
//! state. Nothing about which card is "latest" is kept in memory.
//!
//! * `draft_digest` is an opaque fingerprint of the draft the card showed;
//!   a click on a card whose digest no longer matches the action is stale.
//! * `rendered_status` is the action status the card last showed; a live
//!   card whose action has since moved on is redrawn by the sweep.
//! * `state`: `live` (actionable), `settled` (shows a terminal outcome),
//!   `replaced` (a newer card for the same action superseded it).
//!
//! The action row itself stays the only source of truth for the decision:
//! resolving still goes through the store's compare-and-swap transitions
//! (`try_resolve_action`, `claim_action_for_send`, …), so two surfaces
//! racing on one action get exactly one winner.

use rusqlite::{params, Connection, OptionalExtension};

use crate::store::{Store, StoreError, StoreResult};
use crate::surface::{
    SurfaceAccountRef, SurfaceConversationRef, SurfaceMessageRef, SurfacePlatform,
};

/// Shortest action reference a text command may use. Action IDs are UUIDs,
/// so six hex digits are unique in any realistic queue and short enough to
/// type; anything shorter is treated as an ordinary word.
pub const MIN_ACTION_REF_LEN: usize = 6;

/// Additive, idempotent schema. Called from `Store::migrate`.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS surface_approval_cards (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            action_id TEXT NOT NULL,
            draft_digest TEXT NOT NULL,
            rendered_status TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('live', 'settled', 'replaced')),
            posted_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id, conversation_id, message_id)
        );
        CREATE INDEX IF NOT EXISTS idx_surface_approval_cards_action
        ON surface_approval_cards(platform, action_id);
        CREATE INDEX IF NOT EXISTS idx_surface_approval_cards_state
        ON surface_approval_cards(platform, state);"#,
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalCardState {
    Live,
    Settled,
    Replaced,
}

impl ApprovalCardState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Settled => "settled",
            Self::Replaced => "replaced",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "live" => Some(Self::Live),
            "settled" => Some(Self::Settled),
            "replaced" => Some(Self::Replaced),
            _ => None,
        }
    }
}

/// Where one approval card lives and what it last showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalCardPointer {
    pub message: SurfaceMessageRef,
    pub action_id: String,
    pub draft_digest: String,
    pub rendered_status: String,
    pub state: ApprovalCardState,
    pub posted_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A live card with its action's current truth, for the reconcile sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveApprovalCard {
    pub card: ApprovalCardPointer,
    /// `None` when the action row no longer exists.
    pub action_status: Option<String>,
    pub draft_body: Option<String>,
}

/// What an explicit action reference (`approve 3f2a9c1b`) names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRefMatch {
    None,
    One(String),
    Ambiguous(Vec<String>),
}

const COLUMNS: &str = "c.platform, c.account_id, c.conversation_id, c.message_id, \
    c.action_id, c.draft_digest, c.rendered_status, c.state, c.posted_at_ms, c.updated_at_ms";

const SELECT: &str = "SELECT c.platform, c.account_id, c.conversation_id, c.message_id, \
    c.action_id, c.draft_digest, c.rendered_status, c.state, c.posted_at_ms, c.updated_at_ms \
    FROM surface_approval_cards c";

struct RawCard {
    platform: String,
    account_id: String,
    conversation_id: String,
    message_id: String,
    action_id: String,
    draft_digest: String,
    rendered_status: String,
    state: String,
    posted_at_ms: i64,
    updated_at_ms: i64,
}

fn raw_card(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawCard> {
    Ok(RawCard {
        platform: r.get(0)?,
        account_id: r.get(1)?,
        conversation_id: r.get(2)?,
        message_id: r.get(3)?,
        action_id: r.get(4)?,
        draft_digest: r.get(5)?,
        rendered_status: r.get(6)?,
        state: r.get(7)?,
        posted_at_ms: r.get(8)?,
        updated_at_ms: r.get(9)?,
    })
}

impl RawCard {
    fn parse(self) -> StoreResult<ApprovalCardPointer> {
        let bad = |what: &str| StoreError::InvalidInput(format!("stored approval card: {what}"));
        let platform = SurfacePlatform::new(self.platform).map_err(|e| bad(&e.to_string()))?;
        let account =
            SurfaceAccountRef::new(platform, self.account_id).map_err(|e| bad(&e.to_string()))?;
        let conversation = SurfaceConversationRef::new(account, self.conversation_id, None)
            .map_err(|e| bad(&e.to_string()))?;
        let message = SurfaceMessageRef::new(conversation, self.message_id)
            .map_err(|e| bad(&e.to_string()))?;
        let state = ApprovalCardState::parse(&self.state).ok_or_else(|| bad("state"))?;
        Ok(ApprovalCardPointer {
            message,
            action_id: self.action_id,
            draft_digest: self.draft_digest,
            rendered_status: self.rendered_status,
            state,
            posted_at_ms: self.posted_at_ms,
            updated_at_ms: self.updated_at_ms,
        })
    }
}

fn key(message: &SurfaceMessageRef) -> (&str, &str, &str, &str) {
    let conversation = message.conversation();
    (
        conversation.account().platform().as_str(),
        conversation.account().account_id(),
        conversation.conversation_id(),
        message.message_id(),
    )
}

impl Store {
    /// Record (or re-record) a card this surface posted for `action_id`. The
    /// card is live. Re-recording the same message keeps its first post time.
    pub fn record_approval_card(
        &self,
        message: &SurfaceMessageRef,
        action_id: &str,
        draft_digest: &str,
        rendered_status: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        if action_id.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "approval card needs an action".into(),
            ));
        }
        let (platform, account, conversation, message_id) = key(message);
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO surface_approval_cards
                 (platform, account_id, conversation_id, message_id, action_id, draft_digest,
                  rendered_status, state, posted_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'live', ?8, ?8)
                 ON CONFLICT(platform, account_id, conversation_id, message_id) DO UPDATE SET
                   action_id = excluded.action_id, draft_digest = excluded.draft_digest,
                   rendered_status = excluded.rendered_status, state = 'live',
                   updated_at_ms = excluded.updated_at_ms",
                params![
                    platform,
                    account,
                    conversation,
                    message_id,
                    action_id,
                    draft_digest,
                    rendered_status,
                    now_ms
                ],
            )
        })?;
        Ok(())
    }

    /// The card at `message`, if this store knows it.
    pub fn approval_card(
        &self,
        message: &SurfaceMessageRef,
    ) -> StoreResult<Option<ApprovalCardPointer>> {
        let (platform, account, conversation, message_id) = key(message);
        let raw = self.with_conn(|conn| {
            conn.query_row(
                &format!(
                    "{SELECT} WHERE c.platform = ?1 AND c.account_id = ?2 \
                     AND c.conversation_id = ?3 AND c.message_id = ?4"
                ),
                params![platform, account, conversation, message_id],
                raw_card,
            )
            .optional()
        })?;
        raw.map(RawCard::parse).transpose()
    }

    /// Every card `platform` holds for `action_id`, newest first.
    pub fn approval_cards_for_action(
        &self,
        platform: &SurfacePlatform,
        action_id: &str,
    ) -> StoreResult<Vec<ApprovalCardPointer>> {
        let rows = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{SELECT} WHERE c.platform = ?1 AND c.action_id = ?2 \
                 ORDER BY c.posted_at_ms DESC, c.message_id DESC"
            ))?;
            let rows = stmt
                .query_map(params![platform.as_str(), action_id], raw_card)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.into_iter().map(RawCard::parse).collect()
    }

    /// Record what a card now shows. `false` when the card is unknown.
    pub fn mark_approval_card(
        &self,
        message: &SurfaceMessageRef,
        state: ApprovalCardState,
        draft_digest: &str,
        rendered_status: &str,
        now_ms: i64,
    ) -> StoreResult<bool> {
        let (platform, account, conversation, message_id) = key(message);
        let n = self.with_conn(|conn| {
            conn.execute(
                "UPDATE surface_approval_cards
                 SET state = ?5, draft_digest = ?6, rendered_status = ?7, updated_at_ms = ?8
                 WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3
                   AND message_id = ?4",
                params![
                    platform,
                    account,
                    conversation,
                    message_id,
                    state.as_str(),
                    draft_digest,
                    rendered_status,
                    now_ms
                ],
            )
        })?;
        Ok(n == 1)
    }

    /// Every live card on `platform` with its action's current status and
    /// draft, oldest first. The surface decides which ones need a redraw.
    pub fn live_approval_cards(
        &self,
        platform: &SurfacePlatform,
    ) -> StoreResult<Vec<LiveApprovalCard>> {
        let rows = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS}, a.status, a.draftBody FROM surface_approval_cards c \
                 LEFT JOIN actions a ON a.id = c.action_id \
                 WHERE c.platform = ?1 AND c.state = 'live' \
                 ORDER BY c.posted_at_ms ASC, c.message_id ASC"
            ))?;
            let rows = stmt
                .query_map(params![platform.as_str()], |r| {
                    Ok((
                        raw_card(r)?,
                        r.get::<_, Option<String>>(10)?,
                        r.get::<_, Option<String>>(11)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.into_iter()
            .map(|(raw, action_status, draft_body)| {
                Ok(LiveApprovalCard {
                    card: raw.parse()?,
                    action_status,
                    draft_body,
                })
            })
            .collect()
    }

    /// Resolve an explicit action reference: a full action ID or a unique
    /// prefix of at least [`MIN_ACTION_REF_LEN`] hex digits (dashes allowed),
    /// case-insensitive. Anything else — a short or non-hex word — is
    /// `None`, so ordinary text is never taken for an action.
    pub fn resolve_action_ref(&self, reference: &str) -> StoreResult<ActionRefMatch> {
        let needle = reference.trim().to_ascii_lowercase();
        let hex_digits = needle.chars().filter(|c| c.is_ascii_hexdigit()).count();
        if hex_digits < MIN_ACTION_REF_LEN
            || !needle.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        {
            return Ok(ActionRefMatch::None);
        }
        let ids = self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM actions WHERE lower(id) = ?1 OR substr(lower(id), 1, ?2) = ?1 \
                 ORDER BY createdAt DESC LIMIT 5",
            )?;
            let ids = stmt
                .query_map(params![needle, needle.len() as i64], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })?;
        Ok(match ids.len() {
            0 => ActionRefMatch::None,
            1 => ActionRefMatch::One(ids.into_iter().next().expect("one id")),
            _ => ActionRefMatch::Ambiguous(ids),
        })
    }
}
