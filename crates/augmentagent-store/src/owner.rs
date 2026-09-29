//! #1286 / #1230 — durable owner authority for chat surfaces, keyed by the
//! transport-neutral refs in [`crate::surface`] so Slack, WhatsApp and any
//! later surface share one implementation and one database shape on Linux
//! and macOS.
//!
//! ## Owner binding (`surface_owner_bindings`)
//!
//! One row per `(platform, account)`: the provider user ID the owner
//! confirmed during setup. Authority is always an exact match on this ID
//! inside this account. Display names, emails and push names are never
//! stored here and never consulted.
//!
//! Binding the same owner again only refreshes `confirmed_at_ms`. Binding a
//! different owner replaces the row and drops every control conversation, so
//! a new owner never inherits the old owner's DM or channel.
//!
//! ## Control conversations (`surface_control_conversations`)
//!
//! The conversations where owner input may start a turn: at most one
//! [`ControlConversationKind::Direct`] (the owner's DM with the app) and at
//! most one [`ControlConversationKind::Channel`] (an owner-selected private
//! channel). A conversation holds one kind; setting a kind again moves it.
//! Rows need a binding and are removed with it. Threads are not stored: a
//! thread belongs to its parent conversation.
//!
//! ## Rejection audit (`surface_auth_rejections`)
//!
//! Append-only record of input that was refused before any model, tool or
//! provider call. It stores identifiers and a reason code, never message
//! text, and each field is length-bounded so a hostile payload cannot grow
//! the database without limit.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::store::{Store, StoreError, StoreResult};
use crate::surface::{SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform};

/// Longest identifier or reason code the audit log accepts.
pub const AUDIT_FIELD_MAX: usize = 256;

/// Additive, idempotent schema for this module. Called from `Store::migrate`.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS surface_owner_bindings (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            owner_sender_id TEXT NOT NULL,
            confirmed_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id)
        );
        CREATE TABLE IF NOT EXISTS surface_control_conversations (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('direct', 'channel')),
            added_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id, conversation_id),
            UNIQUE(platform, account_id, kind)
        );
        CREATE TABLE IF NOT EXISTS surface_auth_rejections (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT,
            actor_id TEXT,
            event_kind TEXT NOT NULL,
            event_id TEXT,
            reason TEXT NOT NULL,
            occurred_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_surface_auth_rejections_account
        ON surface_auth_rejections(platform, account_id, id);"#,
    )?;
    Ok(())
}

/// Which role a control conversation plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlConversationKind {
    /// The owner's direct conversation with the app.
    Direct,
    /// An owner-selected private channel.
    Channel,
}

impl ControlConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Channel => "channel",
        }
    }

    fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "direct" => Ok(Self::Direct),
            "channel" => Ok(Self::Channel),
            other => Err(StoreError::InvalidInput(format!(
                "unknown control conversation kind {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlConversation {
    pub conversation: SurfaceConversationRef,
    pub kind: ControlConversationKind,
    pub added_at_ms: i64,
}

/// The bound owner of one surface account and its control conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceOwnerBinding {
    pub owner: SurfaceOwnerRef,
    pub confirmed_at_ms: i64,
    /// Ordered `direct` first, then `channel`.
    pub control: Vec<ControlConversation>,
}

impl SurfaceOwnerBinding {
    fn of_kind(&self, kind: ControlConversationKind) -> Option<&SurfaceConversationRef> {
        self.control
            .iter()
            .find(|c| c.kind == kind)
            .map(|c| &c.conversation)
    }

    pub fn direct_conversation(&self) -> Option<&SurfaceConversationRef> {
        self.of_kind(ControlConversationKind::Direct)
    }

    pub fn control_channel(&self) -> Option<&SurfaceConversationRef> {
        self.of_kind(ControlConversationKind::Channel)
    }

    /// Exact match on the provider conversation ID inside this account.
    pub fn is_control_conversation(&self, conversation_id: &str) -> bool {
        self.control
            .iter()
            .any(|c| c.conversation.conversation_id() == conversation_id)
    }
}

/// One refused input, as handed to [`Store::record_surface_auth_rejection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAuthRejection {
    pub account: SurfaceAccountRef,
    pub conversation_id: Option<String>,
    pub actor_id: Option<String>,
    pub event_kind: String,
    pub event_id: Option<String>,
    /// A stable reason code, not free text.
    pub reason: String,
    pub occurred_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRejection {
    pub id: i64,
    pub account: SurfaceAccountRef,
    pub conversation_id: Option<String>,
    pub actor_id: Option<String>,
    pub event_kind: String,
    pub event_id: Option<String>,
    pub reason: String,
    pub occurred_at_ms: i64,
}

fn write_tx<T>(
    store: &Store,
    f: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
) -> StoreResult<T> {
    store.with_conn(|conn| {
        Ok((|| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })())
    })?
}

fn read<T>(store: &Store, f: impl FnOnce(&Connection) -> StoreResult<T>) -> StoreResult<T> {
    store.with_conn(|conn| Ok(f(conn)))?
}

fn bounded(value: &str, name: &str) -> StoreResult<()> {
    if value.len() > AUDIT_FIELD_MAX {
        Err(StoreError::InvalidInput(format!(
            "{name} is longer than {AUDIT_FIELD_MAX} bytes"
        )))
    } else {
        Ok(())
    }
}

fn account_ref(platform: &str, account_id: &str) -> StoreResult<SurfaceAccountRef> {
    let platform = SurfacePlatform::new(platform)
        .map_err(|e| StoreError::InvalidInput(format!("stored surface platform: {e}")))?;
    SurfaceAccountRef::new(platform, account_id)
        .map_err(|e| StoreError::InvalidInput(format!("stored surface account: {e}")))
}

fn load_binding(
    conn: &Connection,
    account: &SurfaceAccountRef,
) -> StoreResult<Option<SurfaceOwnerBinding>> {
    let platform = account.platform().as_str();
    let account_id = account.account_id();
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT owner_sender_id, confirmed_at_ms FROM surface_owner_bindings \
             WHERE platform = ?1 AND account_id = ?2",
            params![platform, account_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((sender_id, confirmed_at_ms)) = row else {
        return Ok(None);
    };
    let owner = SurfaceOwnerRef::new(account.clone(), sender_id)
        .map_err(|e| StoreError::InvalidInput(format!("stored surface owner: {e}")))?;
    let mut stmt = conn.prepare(
        "SELECT conversation_id, kind, added_at_ms FROM surface_control_conversations \
         WHERE platform = ?1 AND account_id = ?2 \
         ORDER BY CASE kind WHEN 'direct' THEN 0 ELSE 1 END, conversation_id",
    )?;
    let rows = stmt
        .query_map(params![platform, account_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut control = Vec::with_capacity(rows.len());
    for (conversation_id, kind, added_at_ms) in rows {
        let conversation = SurfaceConversationRef::new(account.clone(), conversation_id, None)
            .map_err(|e| StoreError::InvalidInput(format!("stored control conversation: {e}")))?;
        control.push(ControlConversation {
            conversation,
            kind: ControlConversationKind::parse(&kind)?,
            added_at_ms,
        });
    }
    Ok(Some(SurfaceOwnerBinding {
        owner,
        confirmed_at_ms,
        control,
    }))
}

impl Store {
    /// Bind `owner` as the only identity with owner authority on its account.
    /// A different owner than the stored one replaces it and clears the
    /// control conversations; the same owner only refreshes the timestamp.
    pub fn bind_surface_owner(&self, owner: &SurfaceOwnerRef, now_ms: i64) -> StoreResult<()> {
        let account = owner.account();
        let platform = account.platform().as_str();
        let account_id = account.account_id();
        write_tx(self, |tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT owner_sender_id FROM surface_owner_bindings \
                     WHERE platform = ?1 AND account_id = ?2",
                    params![platform, account_id],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.as_deref() != Some(owner.sender_id()) {
                tx.execute(
                    "DELETE FROM surface_control_conversations \
                     WHERE platform = ?1 AND account_id = ?2",
                    params![platform, account_id],
                )?;
            }
            tx.execute(
                "INSERT INTO surface_owner_bindings \
                    (platform, account_id, owner_sender_id, confirmed_at_ms) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(platform, account_id) DO UPDATE SET \
                    owner_sender_id = excluded.owner_sender_id, \
                    confirmed_at_ms = excluded.confirmed_at_ms",
                params![platform, account_id, owner.sender_id(), now_ms],
            )?;
            Ok(())
        })
    }

    pub fn surface_owner_binding(
        &self,
        account: &SurfaceAccountRef,
    ) -> StoreResult<Option<SurfaceOwnerBinding>> {
        read(self, |conn| load_binding(conn, account))
    }

    /// Every binding on one platform, ordered by account ID.
    pub fn surface_owner_bindings(
        &self,
        platform: &SurfacePlatform,
    ) -> StoreResult<Vec<SurfaceOwnerBinding>> {
        read(self, |conn| {
            let mut stmt = conn.prepare(
                "SELECT account_id FROM surface_owner_bindings WHERE platform = ?1 \
                 ORDER BY account_id",
            )?;
            let ids = stmt
                .query_map(params![platform.as_str()], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let account = account_ref(platform.as_str(), &id)?;
                if let Some(binding) = load_binding(conn, &account)? {
                    out.push(binding);
                }
            }
            Ok(out)
        })
    }

    /// Remove the owner binding and its control conversations. Returns
    /// whether a binding existed.
    pub fn unbind_surface_owner(&self, account: &SurfaceAccountRef) -> StoreResult<bool> {
        let platform = account.platform().as_str();
        let account_id = account.account_id();
        write_tx(self, |tx| {
            tx.execute(
                "DELETE FROM surface_control_conversations \
                 WHERE platform = ?1 AND account_id = ?2",
                params![platform, account_id],
            )?;
            let removed = tx.execute(
                "DELETE FROM surface_owner_bindings WHERE platform = ?1 AND account_id = ?2",
                params![platform, account_id],
            )?;
            Ok(removed > 0)
        })
    }

    /// Make `conversation` the account's control conversation of `kind`,
    /// replacing any previous one of that kind. Requires a bound owner.
    pub fn set_surface_control_conversation(
        &self,
        conversation: &SurfaceConversationRef,
        kind: ControlConversationKind,
        now_ms: i64,
    ) -> StoreResult<()> {
        if conversation.thread_id().is_some() {
            return Err(StoreError::InvalidInput(
                "a control conversation cannot be a thread".into(),
            ));
        }
        let account = conversation.account();
        let platform = account.platform().as_str();
        let account_id = account.account_id();
        write_tx(self, |tx| {
            let bound: bool = tx
                .query_row(
                    "SELECT 1 FROM surface_owner_bindings WHERE platform = ?1 AND account_id = ?2",
                    params![platform, account_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !bound {
                return Err(StoreError::InvalidInput(
                    "surface owner is not bound for this account".into(),
                ));
            }
            tx.execute(
                "DELETE FROM surface_control_conversations \
                 WHERE platform = ?1 AND account_id = ?2 \
                   AND (kind = ?3 OR conversation_id = ?4)",
                params![
                    platform,
                    account_id,
                    kind.as_str(),
                    conversation.conversation_id()
                ],
            )?;
            tx.execute(
                "INSERT INTO surface_control_conversations \
                    (platform, account_id, conversation_id, kind, added_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    platform,
                    account_id,
                    conversation.conversation_id(),
                    kind.as_str(),
                    now_ms
                ],
            )?;
            Ok(())
        })
    }

    /// Returns whether the conversation was a control conversation.
    pub fn remove_surface_control_conversation(
        &self,
        conversation: &SurfaceConversationRef,
    ) -> StoreResult<bool> {
        let account = conversation.account();
        write_tx(self, |tx| {
            let removed = tx.execute(
                "DELETE FROM surface_control_conversations \
                 WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3",
                params![
                    account.platform().as_str(),
                    account.account_id(),
                    conversation.conversation_id()
                ],
            )?;
            Ok(removed > 0)
        })
    }

    /// Append one rejection to the audit log and return its row ID.
    pub fn record_surface_auth_rejection(&self, rejection: &NewAuthRejection) -> StoreResult<i64> {
        bounded(&rejection.event_kind, "event kind")?;
        bounded(&rejection.reason, "reason")?;
        for (value, name) in [
            (&rejection.conversation_id, "conversation ID"),
            (&rejection.actor_id, "actor ID"),
            (&rejection.event_id, "event ID"),
        ] {
            if let Some(value) = value {
                bounded(value, name)?;
            }
        }
        let account = &rejection.account;
        write_tx(self, |tx| {
            tx.execute(
                "INSERT INTO surface_auth_rejections \
                    (platform, account_id, conversation_id, actor_id, event_kind, event_id, \
                     reason, occurred_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    account.platform().as_str(),
                    account.account_id(),
                    rejection.conversation_id,
                    rejection.actor_id,
                    rejection.event_kind,
                    rejection.event_id,
                    rejection.reason,
                    rejection.occurred_at_ms,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// The newest `limit` rejections for one account, newest first.
    pub fn surface_auth_rejections(
        &self,
        account: &SurfaceAccountRef,
        limit: usize,
    ) -> StoreResult<Vec<AuthRejection>> {
        read(self, |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, conversation_id, actor_id, event_kind, event_id, reason, occurred_at_ms \
                 FROM surface_auth_rejections WHERE platform = ?1 AND account_id = ?2 \
                 ORDER BY id DESC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(
                    params![
                        account.platform().as_str(),
                        account.account_id(),
                        i64::try_from(limit).unwrap_or(i64::MAX)
                    ],
                    |r| {
                        Ok(AuthRejection {
                            id: r.get(0)?,
                            account: account.clone(),
                            conversation_id: r.get(1)?,
                            actor_id: r.get(2)?,
                            event_kind: r.get(3)?,
                            event_id: r.get(4)?,
                            reason: r.get(5)?,
                            occurred_at_ms: r.get(6)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn surface_auth_rejection_count(&self, account: &SurfaceAccountRef) -> StoreResult<i64> {
        read(self, |conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM surface_auth_rejections WHERE platform = ?1 AND account_id = ?2",
                params![account.platform().as_str(), account.account_id()],
                |r| r.get(0),
            )?)
        })
    }
}
