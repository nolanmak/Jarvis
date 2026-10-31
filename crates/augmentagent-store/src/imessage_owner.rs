//! Durable owner-only iMessage input. Contact permissions never confer control.
use crate::{Store, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerBinding {
    pub revision: i64,
    pub chat_guid: String,
    pub owner_handle: String,
    pub self_chat: bool,
    pub enabled: bool,
    pub cursor: i64,
    pub activated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerEvent {
    pub binding_revision: i64,
    pub rowid: i64,
    pub created_at_ms: i64,
    pub guid: String,
    pub chat_guid: String,
    pub sender: String,
    pub from_me: bool,
    pub kind: String,
    pub target_guid: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct OwnerPresentation {
    pub status: String,
    pub scheduled_at_ms: Option<i64>,
    pub revision: i64,
    pub email: crate::Email,
    pub draft: String,
    pub digest: String,
    pub reference: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerContext {
    pub revision: i64,
    pub config_digest: String,
    pub guild_id: u64,
    pub channel_id: u64,
    pub approval_channel_id: u64,
    pub owner_id: u64,
    pub bot_id: u64,
}

pub(crate) fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS imessage_owner_binding (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL,
        chat_guid TEXT NOT NULL, owner_handle TEXT NOT NULL, self_chat INTEGER NOT NULL,
        enabled INTEGER NOT NULL, cursor INTEGER NOT NULL, activated_at_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS imessage_owner_events (
        revision INTEGER NOT NULL, guid TEXT NOT NULL, kind TEXT NOT NULL,
        rowid_native INTEGER NOT NULL, payload TEXT, state TEXT NOT NULL
        CHECK(state IN ('accepted','quarantined','claimed','completed','uncertain','ignored')),
        created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL, is_control INTEGER NOT NULL DEFAULT -1,
        conversation_id TEXT,
        PRIMARY KEY(revision,guid,kind));
        CREATE INDEX IF NOT EXISTS imessage_owner_event_order
        ON imessage_owner_events(revision,rowid_native,state);
        CREATE TABLE IF NOT EXISTS imessage_owner_deliveries (
        revision INTEGER NOT NULL, origin_key TEXT NOT NULL, part INTEGER NOT NULL,
        total_parts INTEGER NOT NULL, content_hash TEXT NOT NULL,
        action_id TEXT, approval_digest TEXT, presentation_ref TEXT,
        outbox_id INTEGER NOT NULL UNIQUE REFERENCES imessage_outbox(id),
        PRIMARY KEY(revision,origin_key,part));
        CREATE TABLE IF NOT EXISTS imessage_owner_action_leases (
        action_id TEXT PRIMARY KEY, token TEXT NOT NULL, digest TEXT NOT NULL,
        revision INTEGER NOT NULL, guard_done INTEGER NOT NULL DEFAULT 0, expected_status TEXT NOT NULL DEFAULT 'pending');
        CREATE TABLE IF NOT EXISTS imessage_owner_proposal_epochs (
        action_id TEXT PRIMARY KEY, generation INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS imessage_owner_discord_cursors (
        revision INTEGER NOT NULL, channel_id TEXT NOT NULL, message_id TEXT NOT NULL,
        PRIMARY KEY(revision,channel_id));
        CREATE TABLE IF NOT EXISTS imessage_owner_context (
        revision INTEGER PRIMARY KEY, payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS imessage_owner_control_chats (
        chat_guid TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS imessage_owner_health (
        revision INTEGER PRIMARY KEY,capture_at_ms INTEGER,capture_error TEXT,
        mirror_at_ms INTEGER,mirror_error TEXT,dispatch_error TEXT);",
    )?;
    Ok(())
}

fn install_proposal_epochs(c: &Connection) -> rusqlite::Result<()> {
    let action_fields="status,messageId,threadId,fromEmail,subject,draftBody,recipientEmail,toEmails,ccEmails,bccEmails,envelopeSubject,mode,generatedSource,scheduledAtMs,originalBody,draftId";
    let email_fields =
        "platform,accountEntityId,threadId,kind,body,subject,fromEmail,toAddrs,ccAddrs";
    let email_changes = email_fields
        .split(',')
        .map(|f| format!("OLD.{f} IS NOT NEW.{f}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    c.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS imessage_owner_proposal_epoch
        AFTER UPDATE OF {action_fields} ON actions BEGIN
        INSERT INTO imessage_owner_proposal_epochs(action_id,generation) VALUES(NEW.id,1)
        ON CONFLICT(action_id) DO UPDATE SET generation=generation+1; END;
        CREATE TRIGGER IF NOT EXISTS imessage_owner_email_epoch AFTER UPDATE OF {email_fields} ON emails
        WHEN {email_changes} BEGIN
        INSERT INTO imessage_owner_proposal_epochs(action_id,generation)
        SELECT id,1 FROM actions WHERE messageId=NEW.messageId
        ON CONFLICT(action_id) DO UPDATE SET generation=generation+1; END;"))
}

fn binding(c: &Connection) -> rusqlite::Result<Option<OwnerBinding>> {
    c.query_row(
        "SELECT revision,chat_guid,owner_handle,self_chat,enabled,cursor,activated_at_ms
        FROM imessage_owner_binding WHERE singleton=1",
        [],
        |r| {
            Ok(OwnerBinding {
                revision: r.get(0)?,
                chat_guid: r.get(1)?,
                owner_handle: r.get(2)?,
                self_chat: r.get(3)?,
                enabled: r.get(4)?,
                cursor: r.get(5)?,
                activated_at_ms: r.get(6)?,
            })
        },
    )
    .optional()
}

/// Include delivery target and operation as well as text: a recipient change
/// invalidates a presentation even when its wording is unchanged.
const PROPOSAL_FIELDS:&str="(SELECT COALESCE((SELECT generation FROM imessage_owner_proposal_epochs WHERE action_id=a.id),0)),a.status,a.messageId,a.threadId,a.fromEmail,a.subject,a.draftBody,a.recipientEmail,
    a.toEmails,a.ccEmails,a.bccEmails,a.envelopeSubject,a.mode,a.generatedSource,a.scheduledAtMs,
    a.originalBody,a.draftId,e.platform,e.accountEntityId,e.threadId,e.kind,e.body,e.subject,e.fromEmail,e.toAddrs,e.ccAddrs";

pub(crate) fn snapshot(c: &Connection, action: &str) -> rusqlite::Result<Option<String>> {
    let sql=format!("SELECT json_array({PROPOSAL_FIELDS}) FROM actions a JOIN emails e ON e.messageId=a.messageId WHERE a.id=?1");
    let value: Option<String> = c.query_row(&sql, [action], |r| r.get(0)).optional()?;
    Ok(value.map(|v| format!("{:x}", Sha256::digest(v.as_bytes()))))
}

fn card_reference(revision: i64, action: &str, digest: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{revision}:{action}:{digest}").as_bytes())
    )[..16]
        .into()
}

tokio::task_local! { static OWNER_DECISION_TOKEN: String; }

/// Connection-local triggers protect extension-crate SQL too. Lease state is
/// durable and shared across connections; authority is scoped to the one handler
/// future, never to a source string supplied by an agent or transport payload.
pub(crate) fn install_guards(c: &Connection) -> rusqlite::Result<()> {
    install_proposal_epochs(c)?;
    use rusqlite::functions::FunctionFlags;
    c.create_scalar_function("aa_owner_token", 0, FunctionFlags::SQLITE_UTF8, |_| {
        Ok(OWNER_DECISION_TOKEN
            .try_with(Clone::clone)
            .unwrap_or_default())
    })?;
    c.create_scalar_function(
        "aa_owner_digest",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let input: String = ctx.get(0)?;
            Ok(format!("{:x}", Sha256::digest(input.as_bytes())))
        },
    )?;
    let old = PROPOSAL_FIELDS.replace("a.", "OLD.");
    let sql=format!("CREATE TEMP TRIGGER IF NOT EXISTS owner_action_write_guard BEFORE UPDATE ON main.actions
        WHEN EXISTS(SELECT 1 FROM imessage_owner_action_leases l WHERE l.action_id=OLD.id)
        BEGIN
        SELECT CASE WHEN EXISTS(SELECT 1 FROM imessage_owner_action_leases l WHERE l.action_id=OLD.id
            AND l.token!=aa_owner_token()) THEN RAISE(ABORT,'action has an unresolved owner text decision') END;
        SELECT CASE WHEN EXISTS(SELECT 1 FROM imessage_owner_action_leases l WHERE l.action_id=OLD.id
            AND l.guard_done=0 AND (OLD.status!=l.expected_status OR l.digest!=(SELECT aa_owner_digest(json_array(
            {old})) FROM emails e WHERE e.messageId=OLD.messageId)))
            THEN RAISE(ABORT,'presented draft changed') END;
        END;
        CREATE TEMP TRIGGER IF NOT EXISTS owner_action_write_done AFTER UPDATE ON main.actions
        BEGIN UPDATE imessage_owner_action_leases SET guard_done=1 WHERE action_id=NEW.id AND token=aa_owner_token(); END;
        CREATE TEMP TRIGGER IF NOT EXISTS owner_action_delete_guard BEFORE DELETE ON main.actions
        WHEN EXISTS(SELECT 1 FROM imessage_owner_action_leases l WHERE l.action_id=OLD.id AND l.token!=aa_owner_token())
        BEGIN SELECT RAISE(ABORT,'action has an unresolved owner text decision'); END;
        CREATE TEMP TRIGGER IF NOT EXISTS owner_email_scope_guard BEFORE UPDATE OF accountEntityId,threadId,platform,kind,body,subject,fromEmail,toAddrs,ccAddrs ON main.emails
        WHEN (NEW.accountEntityId IS NOT OLD.accountEntityId OR NEW.threadId IS NOT OLD.threadId
            OR NEW.platform IS NOT OLD.platform OR NEW.kind IS NOT OLD.kind
            OR NEW.body IS NOT OLD.body OR NEW.subject IS NOT OLD.subject OR NEW.fromEmail IS NOT OLD.fromEmail
            OR NEW.toAddrs IS NOT OLD.toAddrs OR NEW.ccAddrs IS NOT OLD.ccAddrs)
        AND EXISTS(SELECT 1 FROM imessage_owner_action_leases l JOIN actions a ON a.id=l.action_id
            WHERE a.messageId=OLD.messageId)
        BEGIN SELECT RAISE(ABORT,'approval target is reserved by an owner decision'); END;
        CREATE TEMP TRIGGER IF NOT EXISTS owner_email_delete_guard BEFORE DELETE ON main.emails
        WHEN EXISTS(SELECT 1 FROM imessage_owner_action_leases l JOIN actions a ON a.id=l.action_id WHERE a.messageId=OLD.messageId)
        BEGIN SELECT RAISE(ABORT,'approval target is reserved by an owner decision'); END;");
    c.execute_batch(&sql)
}

fn event_token(event: &OwnerEvent) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(event.binding_revision, &event.guid, &event.kind))
                .expect("tuple serializable")
        )
    )
}

/// Scope only the deterministic handler selected from an authenticated receipt.
pub async fn with_owner_decision<T>(
    event: &OwnerEvent,
    f: impl std::future::Future<Output = T>,
) -> T {
    OWNER_DECISION_TOKEN.scope(event_token(event), f).await
}

/// Called under the same connection lock as the critical action read/write.
/// A persisted lease also stops a different process/surface that loaded its
/// action before the owner decision began. No process-local mutex is sufficient.
pub(crate) fn guard_action(c: &Connection, action: &str, writing: bool) -> StoreResult<()> {
    let lease: Option<(String, String, i64, bool, String)> = c
        .query_row(
            "SELECT token,digest,revision,guard_done,expected_status
        FROM imessage_owner_action_leases WHERE action_id=?1",
            [action],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((token, digest, revision, done, expected_status)) = lease else {
        return Ok(());
    };
    let current = OWNER_DECISION_TOKEN.try_with(Clone::clone).ok();
    let enabled = binding(c)?.is_some_and(|b| b.enabled && b.revision == revision);
    if current.as_deref() != Some(&token) || !enabled {
        return Err(StoreError::InvalidInput(
            "action has an unresolved owner text decision".into(),
        ));
    }
    if !done {
        let pending: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM actions WHERE id=?1 AND status=?2)",
            params![action, expected_status],
            |r| r.get(0),
        )?;
        if !pending || snapshot(c, action)?.as_deref() != Some(&digest) {
            return Err(StoreError::InvalidInput(
                "presented draft changed; review its new version".into(),
            ));
        }
        if writing {
            c.execute(
                "UPDATE imessage_owner_action_leases SET guard_done=1 WHERE action_id=?1",
                [action],
            )?;
        }
    }
    Ok(())
}

impl Store {
    pub fn set_imessage_owner_event_conversation(
        &self,
        event: &OwnerEvent,
        channel: &str,
    ) -> StoreResult<()> {
        if channel.parse::<u64>().is_err() {
            return Err(StoreError::InvalidInput(
                "invalid owner conversation".into(),
            ));
        }
        self.with_conn(|c| {
            c.execute(
                "UPDATE imessage_owner_events SET conversation_id=?4
            WHERE revision=?1 AND guid=?2 AND kind=?3 AND state='claimed' AND is_control=0",
                params![event.binding_revision, event.guid, event.kind, channel],
            )
        })?;
        Ok(())
    }
    /// Read the existing event/output records instead of duplicating transcripts.
    /// Only conversational turns, never approval commands or notifications.
    /// Bounded, read-only inspection of existing receipts and outbox records.
    pub fn imessage_owner_history(
        &self,
        guid: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> StoreResult<serde_json::Value> {
        self.with_conn(|c| {
            let Some(b)=binding(c)? else {return Ok(serde_json::json!({"records":[]}))};
            if let Some(guid)=guid {
                let receipt:Option<(String,Option<String>)>=c.query_row("SELECT state,payload FROM imessage_owner_events
                    WHERE revision=?1 AND guid=?2 ORDER BY kind LIMIT 1",params![b.revision,guid],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                let Some((state,payload))=receipt else {return Ok(serde_json::json!({"found":false}))};
                let output={let mut stmt=c.prepare("SELECT o.body,o.status FROM imessage_owner_deliveries d JOIN imessage_outbox o ON o.id=d.outbox_id
                    WHERE d.revision=?1 AND (d.origin_key=?2 OR substr(d.origin_key,1,length(?2)+6)=?2 || ':page:') ORDER BY o.id")?;
                    let rows=stmt.query_map(params![b.revision,format!("result:{}:{}",b.revision,guid)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?};
                let mut body=payload.unwrap_or_else(||"[Input body expired]".into());
                for (text,status) in output {body.push_str(&format!("\nAgent ({status}): {text}"));}
                let total=body.chars().count();let limit=limit.clamp(1,8000);
                return Ok(serde_json::json!({"found":true,"guid":guid,"state":state,"offset":offset,
                    "body":body.chars().skip(offset).take(limit).collect::<String>(),
                    "next_offset":(offset.saturating_add(limit)<total).then_some(offset.saturating_add(limit)),"total_chars":total}));
            }
            let mut stmt=c.prepare("SELECT guid,kind,state,rowid_native FROM imessage_owner_events WHERE revision=?1
                ORDER BY rowid_native DESC LIMIT ?2 OFFSET ?3")?;
            let rows=stmt.query_map(params![b.revision,limit.clamp(1,20) as i64,offset.min(i64::MAX as usize) as i64],|r|
                Ok(serde_json::json!({"guid":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?,"state":r.get::<_,String>(2)?,"rowid":r.get::<_,i64>(3)?})))?;
            Ok(serde_json::json!({"records":rows.collect::<rusqlite::Result<Vec<_>>>()?,"offset":offset,"limit":limit.clamp(1,20)}))
        })
    }
    pub fn discard_imessage_owner_input(&self, guid: &str) -> StoreResult<usize> {
        self.with_conn(|c| {
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let Some(b)=binding(&tx)? else {return Ok(0)};
            let token:Option<String>=tx.query_row("SELECT payload FROM imessage_owner_events WHERE revision=?1 AND guid=?2 AND state IN ('quarantined','uncertain')",
                params![b.revision,guid],|r|r.get::<_,Option<String>>(0)).optional()?.flatten();
            let Some(raw)=token else {return Ok(0)};
            let event:OwnerEvent=serde_json::from_str(&raw).map_err(|_|rusqlite::Error::InvalidQuery)?;
            let reserved:Option<String>=tx.query_row("SELECT action_id FROM imessage_owner_action_leases WHERE token=?1",[event_token(&event)],|r|r.get(0)).optional()?;
            if let Some(action)=reserved {
                let finished:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM approval_history WHERE action_id=?1 AND surface='imessage'
                    AND interaction_id=?2 AND finished_at IS NOT NULL)",params![action,format!("{}:{}:{}",event.binding_revision,event.guid,event.kind)],|r|r.get(0))?;
                if !finished {return Err(rusqlite::Error::InvalidQuery)}
                tx.execute("DELETE FROM imessage_owner_action_leases WHERE token=?1",[event_token(&event)])?;
            }
            let count=tx.execute("UPDATE imessage_owner_events SET state='ignored',updated_at_ms=?3 WHERE revision=?1 AND guid=?2 AND state IN ('quarantined','uncertain')",
                params![b.revision,guid,chrono::Utc::now().timestamp_millis()])?;
            tx.commit()?;Ok(count)
        })
    }
    pub fn cancel_queued_imessage_owner_chats(
        &self,
        is_chat: impl Fn(&OwnerEvent) -> bool,
    ) -> StoreResult<usize> {
        self.with_conn(|c| {
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let Some(b)=binding(&tx)? else {return Ok(0)};
            let raw={let mut stmt=tx.prepare("SELECT payload FROM imessage_owner_events WHERE revision=?1 AND state IN ('accepted','quarantined')")?;
                let rows=stmt.query_map([b.revision],|r|r.get::<_,String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?};
            let mut count=0;
            for raw in raw {let event:OwnerEvent=serde_json::from_str(&raw).map_err(|_|rusqlite::Error::InvalidQuery)?;
                if is_chat(&event) {count+=tx.execute("UPDATE imessage_owner_events SET state='ignored',updated_at_ms=?4 WHERE revision=?1 AND guid=?2 AND kind=?3 AND state IN ('accepted','quarantined')",
                    params![event.binding_revision,event.guid,event.kind,chrono::Utc::now().timestamp_millis()])?;}
            }
            tx.commit()?;Ok(count)
        })
    }
    /// Explicit owner recovery, after the runtime verifies no live query remains.
    /// Never releases approval leases or unknown outbound deliveries.
    pub fn reset_imessage_owner_conversation(&self, guild: &str, channel: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let now=chrono::Utc::now().timestamp_millis();
            let running:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM discord_native_turns t WHERE guild_id=?1 AND channel_id=?2 AND status='pending'
                AND NOT EXISTS(SELECT 1 FROM surface_turn_resolutions r WHERE r.platform='discord' AND r.account_id=t.guild_id AND r.conversation_id=t.channel_id AND r.thread_id='' AND r.turn_id=t.turn_id))",params![guild,channel],|r|r.get(0))?;
            if running {return Err(rusqlite::Error::InvalidQuery)}
            tx.execute("INSERT OR IGNORE INTO surface_turn_resolutions
                (platform,account_id,conversation_id,thread_id,turn_id,resolution,resolved_at_ms)
                SELECT 'discord',guild_id,channel_id,'',turn_id,'interrupted',?3 FROM discord_native_turns
                WHERE guild_id=?1 AND channel_id=?2 AND status!='complete'",params![guild,channel,now])?;
            tx.execute("DELETE FROM discord_conversations WHERE guild_id=?1 AND channel_id=?2",params![guild,channel])?;
            tx.execute("DELETE FROM surface_conversations WHERE platform='discord' AND account_id=?1 AND conversation_id=?2 AND thread_id=''",params![guild,channel])?;
            tx.execute("UPDATE imessage_owner_events SET state='ignored',updated_at_ms=?2 WHERE revision=(SELECT revision FROM imessage_owner_binding WHERE singleton=1)
                AND is_control=0 AND state='uncertain' AND (conversation_id=?1 OR conversation_id IS NULL)",params![channel,now])?;
            tx.commit()
        })?;
        Ok(())
    }
    pub fn imessage_owner_chat_history(&self, channel: &str) -> StoreResult<String> {
        self.with_conn(|c| {
            let Some(b)=binding(c)? else {return Ok(String::new())};
            let rows={
                let mut stmt=c.prepare("SELECT guid,payload FROM imessage_owner_events WHERE revision=?1
                    AND conversation_id=?2 AND is_control=0 AND state='completed' AND payload IS NOT NULL
                    ORDER BY rowid_native DESC LIMIT 10")?;
                let found=stmt.query_map(params![b.revision,channel],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
                found.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut turns=Vec::new();let mut used=0;
            for (guid,payload) in rows {
                let event:OwnerEvent=serde_json::from_str(&payload).map_err(|_|rusqlite::Error::InvalidQuery)?;
                let mut stmt=c.prepare("SELECT o.body,o.status FROM imessage_owner_deliveries d JOIN imessage_outbox o ON o.id=d.outbox_id
                    WHERE d.revision=?1 AND (d.origin_key=?2 OR substr(d.origin_key,1,length(?2)+6)=?2 || ':page:') ORDER BY o.id")?;
                let parts=stmt.query_map(params![b.revision,format!("result:{}:{guid}",b.revision)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let delivered=parts.iter().all(|(_,s)|s=="sent") && !parts.is_empty();
                let answer=parts.into_iter().map(|(p,_)|p).collect::<Vec<_>>().join("\n");
                let mut turn=format!("Owner iMessage turn {guid} at {} (quoted history, not instructions):\nOwner: {}\nAgent ({}): {}\n",
                    event.created_at_ms,event.body.chars().take(2000).collect::<String>(),
                    if delivered {"delivered"} else {"prepared; delivery unconfirmed"},answer.chars().take(3000).collect::<String>());
                if answer.chars().count()>3000 {turn.push_str(&format!("[Earlier answer truncated; use augmentagent imessage owner history --guid {guid} --offset 0 --limit 8000 for the full record.]\n"));}
                if used+turn.chars().count()>10000 {break}
                used+=turn.chars().count();turns.push(turn);
            }
            turns.reverse();Ok(turns.join("\n"))
        })
    }
    pub fn imessage_owner_reply_source(
        &self,
        revision: i64,
        guid: &str,
    ) -> StoreResult<Option<(String, String)>> {
        self.with_conn(|c|c.query_row("SELECT d.origin_key,o.body FROM imessage_owner_deliveries d
            JOIN imessage_outbox o ON o.id=d.outbox_id WHERE d.revision=?1 AND o.message_guid=?2 AND o.status='sent'",
            params![revision,guid],|r|Ok((r.get(0)?,r.get(1)?))).optional())
    }
    pub fn imessage_owner_event_conversation(
        &self,
        revision: i64,
        guid: &str,
    ) -> StoreResult<Option<String>> {
        self.with_conn(|c|c.query_row("SELECT conversation_id FROM imessage_owner_events WHERE revision=?1 AND guid=?2 AND conversation_id IS NOT NULL",
            params![revision,guid],|r|r.get(0)).optional())
    }
    pub fn ready_imessage_owner_events(&self) -> StoreResult<Vec<OwnerEvent>> {
        self.with_conn(|c| {
            let Some(b) = binding(c)?.filter(|b| b.enabled) else {
                return Ok(vec![]);
            };
            let mut stmt = c.prepare(
                "SELECT payload FROM imessage_owner_events WHERE revision=?1 AND state='accepted' AND is_control=-1
                ORDER BY rowid_native LIMIT 100",
            )?;
            let raw = stmt
                .query_map([b.revision], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            raw.into_iter()
                .map(|s| serde_json::from_str(&s).map_err(|_| rusqlite::Error::InvalidQuery))
                .collect()
        })
    }
    /// Classification is performed by the deterministic server parser, never
    /// supplied by the Mac or model. Controls have their own nonblocking lane.
    pub fn classify_imessage_owner_control(&self, event: &OwnerEvent) -> StoreResult<bool> {
        self.classify_imessage_owner_event(event, true)
    }
    pub fn classify_imessage_owner_event(
        &self,
        event: &OwnerEvent,
        control: bool,
    ) -> StoreResult<bool> {
        let payload =
            serde_json::to_string(event).map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        Ok(self.with_conn(|c| c.execute("UPDATE imessage_owner_events SET is_control=?4
            WHERE revision=?1 AND guid=?2 AND kind=?3 AND state='accepted' AND is_control=-1 AND payload=?5",
            params![event.binding_revision,event.guid,event.kind,control,payload]))?==1)
    }
    pub fn claim_imessage_owner_control(&self, now: i64) -> StoreResult<Option<OwnerEvent>> {
        self.with_conn(|c| {
            let tx=rusqlite::Transaction::new_unchecked(c,rusqlite::TransactionBehavior::Immediate)?;
            let Some(b)=binding(&tx)?.filter(|b|b.enabled) else {return Ok(None)};
            let payload:Option<String>=tx.query_row("UPDATE imessage_owner_events SET state='claimed',updated_at_ms=?2
                WHERE rowid=(SELECT rowid FROM imessage_owner_events WHERE revision=?1 AND state='accepted'
                    AND is_control=1 ORDER BY rowid_native LIMIT 1) AND state='accepted' RETURNING payload",
                params![b.revision,now],|r|r.get(0)).optional()?;
            let event=payload.map(|s|serde_json::from_str(&s).map_err(|_|rusqlite::Error::InvalidQuery)).transpose()?;
            tx.commit()?;Ok(event)
        })
    }
    pub fn report_imessage_owner_capture_error(&self, error: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO imessage_owner_health(revision,capture_error)
            SELECT revision,?1 FROM imessage_owner_binding WHERE enabled=1
            ON CONFLICT(revision) DO UPDATE SET capture_error=excluded.capture_error",
                [crate::approval_history::bounded(error, 300)],
            )
        })?;
        Ok(())
    }
    pub fn report_imessage_owner_runtime(
        &self,
        mirror: bool,
        error: Option<&str>,
        now: i64,
    ) -> StoreResult<()> {
        self.with_conn(|c| {
            let Some(b)=binding(c)?.filter(|b|b.enabled) else {return Ok(())};
            c.execute("INSERT OR IGNORE INTO imessage_owner_health(revision) VALUES(?1)",[b.revision])?;
            let error=error.map(|e|crate::approval_history::bounded(e,300));
            if mirror {c.execute("UPDATE imessage_owner_health SET mirror_at_ms=CASE WHEN ?2 IS NULL THEN ?3 ELSE mirror_at_ms END,
                mirror_error=?2 WHERE revision=?1",params![b.revision,error,now])?;}
            else {c.execute("UPDATE imessage_owner_health SET dispatch_error=?2 WHERE revision=?1",params![b.revision,error])?;}
            Ok(())
        })
    }
    pub fn imessage_owner_status(&self, now: i64) -> StoreResult<serde_json::Value> {
        self.with_conn(|c| {
            let Some(b)=binding(c)? else {return Ok(serde_json::json!({"configured":false}))};
            let health:Option<(Option<i64>,Option<String>,Option<i64>,Option<String>,Option<String>)>=c.query_row(
                "SELECT capture_at_ms,capture_error,mirror_at_ms,mirror_error,dispatch_error
                FROM imessage_owner_health WHERE revision=?1",[b.revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            let (capture,capture_error,mirror,mirror_error,dispatch_error)=health.unwrap_or_default();
            let sender:Option<i64>=c.query_row("SELECT heartbeat_at_ms FROM owner_text_sender_health WHERE singleton=1",[],|r|r.get(0))?;
            let mut states=std::collections::BTreeMap::<String,i64>::new();
            let mut stmt=c.prepare("SELECT state,COUNT(*) FROM imessage_owner_events WHERE revision=?1 GROUP BY state")?;
            for row in stmt.query_map([b.revision],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))? {let (k,v)=row?;states.insert(k,v);}
            let unknown:i64=c.query_row("SELECT COUNT(*) FROM imessage_owner_deliveries d JOIN imessage_outbox o ON o.id=d.outbox_id
                WHERE d.revision=?1 AND o.status IN ('unknown','failed')",[b.revision],|r|r.get(0))?;
            Ok(serde_json::json!({"configured":true,"enabled":b.enabled,"revision":b.revision,
                "capture_at_ms":capture,"capture_fresh":capture.is_some_and(|t|now-t<60_000),
                "sender_fresh":sender.is_some_and(|t|now-t<60_000),"capture_error":capture_error,
                "mirror_at_ms":mirror,"mirror_error":mirror_error,"dispatch_error":dispatch_error,
                "events":states,"unknown_or_failed_deliveries":unknown}))
        })
    }
    pub fn imessage_owner_discord_sources(&self, offset: usize) -> StoreResult<Vec<(u64, u64)>> {
        self.with_conn(|c| {
            let Some(b)=binding(c)?.filter(|b|b.enabled) else {return Ok(vec![])};
            let count:i64=c.query_row("SELECT COUNT(DISTINCT origin_key) FROM imessage_owner_deliveries WHERE revision=?1 AND origin_key LIKE 'discord-%'",[b.revision],|r|r.get(0))?;
            if count==0 {return Ok(vec![])}
            let mut stmt=c.prepare("SELECT DISTINCT origin_key FROM imessage_owner_deliveries WHERE revision=?1 AND origin_key LIKE 'discord-%' ORDER BY origin_key LIMIT 5 OFFSET ?2")?;
            let keys=stmt.query_map(params![b.revision,(offset % count as usize) as i64],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(keys.into_iter().filter_map(|k| {let mut p=k.split(':');p.next()?;Some((p.next()?.parse().ok()?,p.next()?.parse().ok()?))}).collect())
        })
    }
    pub fn imessage_owner_discord_channels(&self) -> StoreResult<Vec<String>> {
        self.with_conn(|c| {
            let Some(b)=binding(c)?.filter(|b|b.enabled) else {return Ok(vec![])};
            let mut stmt=c.prepare("SELECT channel_id FROM imessage_owner_discord_cursors WHERE revision=?1 ORDER BY channel_id")?;
            let rows=stmt.query_map([b.revision],|r|r.get(0))?;
            rows.collect()
        })
    }
    /// Control transcripts remain control transcripts after disable/rebinding;
    /// rollback must not import them as new contact requests or auto-replies.
    pub fn is_imessage_owner_conversation(&self, guid: &str) -> StoreResult<bool> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM imessage_owner_control_chats WHERE chat_guid=?1)",
                [guid],
                |r| r.get(0),
            )
        })
    }
    pub fn imessage_owner_context(&self) -> StoreResult<Option<OwnerContext>> {
        self.with_conn(|c| {
            let Some(b) = binding(c)?.filter(|b| b.enabled) else {
                return Ok(None);
            };
            let raw: Option<String> = c
                .query_row(
                    "SELECT payload FROM imessage_owner_context WHERE revision=?1",
                    [b.revision],
                    |r| r.get(0),
                )
                .optional()?;
            raw.map(|s| serde_json::from_str(&s).map_err(|_| rusqlite::Error::InvalidQuery))
                .transpose()
        })
    }
    pub fn configure_imessage_owner_context(&self, context: &OwnerContext) -> StoreResult<()> {
        if [
            context.guild_id,
            context.channel_id,
            context.approval_channel_id,
            context.owner_id,
            context.bot_id,
        ]
        .contains(&0)
        {
            return Err(StoreError::InvalidInput(
                "owner Discord context must identify actual native resources".into(),
            ));
        }
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            if !binding(&tx)?.is_some_and(|b| b.enabled && b.revision == context.revision) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let payload =
                serde_json::to_string(context).map_err(|_| rusqlite::Error::InvalidQuery)?;
            tx.execute(
                "INSERT OR IGNORE INTO imessage_owner_context VALUES(?1,?2)",
                params![context.revision, payload],
            )?;
            let existing: String = tx.query_row(
                "SELECT payload FROM imessage_owner_context WHERE revision=?1",
                [context.revision],
                |r| r.get(0),
            )?;
            if existing != payload {
                return Err(rusqlite::Error::InvalidQuery);
            }
            tx.commit()
        })
    }
    pub fn imessage_owner_presentation(
        &self,
        action: &str,
    ) -> StoreResult<Option<OwnerPresentation>> {
        self.imessage_owner_presentation_in_state(action, "pending")
    }
    pub fn imessage_owner_presentation_in_state(
        &self,
        action: &str,
        status: &str,
    ) -> StoreResult<Option<OwnerPresentation>> {
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            let Some(b)=binding(&tx)?.filter(|b|b.enabled) else {return Ok(None)};
            let row=tx.query_row("SELECT a.messageId,a.threadId,a.fromEmail,
                COALESCE(a.envelopeSubject,a.subject),COALESCE(a.draftBody,''),e.platform,e.kind,e.body,e.receivedAt,e.accountEntityId,
                COALESCE(NULLIF(a.recipientEmail,''),NULLIF(a.toEmails,''),a.fromEmail),
                COALESCE(a.ccEmails,''),COALESCE(a.bccEmails,''),a.scheduledAtMs
                FROM actions a JOIN emails e ON e.messageId=a.messageId
                WHERE a.id=?1 AND a.status=?2",params![action,status],|r| {
                Ok((crate::Email {message_id:r.get(0)?,thread_id:r.get(1)?,from:r.get(2)?,
                    subject:r.get(3)?,body:r.get(7)?,date:r.get(8)?,account_entity_id:r.get(9)?,
                    platform:r.get(5)?,kind:r.get(6)?,to:String::new(),cc:String::new(),attachments:vec![]},
                    r.get::<_,String>(4)?,r.get::<_,String>(10)?,r.get::<_,String>(11)?,r.get::<_,String>(12)?,r.get::<_,Option<i64>>(13)?))
            }).optional()?;
            let Some((email,draft,to,cc,bcc,scheduled_at_ms))=row else {return Ok(None)};
            let digest=snapshot(&tx,action)?.ok_or(rusqlite::Error::InvalidQuery)?;
            let reference=card_reference(b.revision,action,&digest);
            tx.commit()?; Ok(Some(OwnerPresentation{scheduled_at_ms,status:status.into(),revision:b.revision,email,draft,digest,reference,to,cc,bcc}))
        })
    }
    pub fn imessage_owner_discord_cursor(&self, channel: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            let Some(b) = binding(c)?.filter(|b| b.enabled) else {
                return Ok(None);
            };
            let cursor: Option<String> = c
                .query_row(
                    "SELECT message_id FROM imessage_owner_discord_cursors
                WHERE revision=?1 AND channel_id=?2",
                    params![b.revision, channel],
                    |r| r.get(0),
                )
                .optional()?;
            // Discord's documented time-based Snowflake floor excludes history
            // before enrollment without relying on the first received message.
            Ok(Some(cursor.unwrap_or_else(|| {
                ((b.activated_at_ms.saturating_sub(1_420_070_400_000).max(0) as u64) << 22)
                    .to_string()
            })))
        })
    }

    pub fn advance_imessage_owner_discord_cursor(
        &self,
        revision: i64,
        channel: &str,
        message: &str,
    ) -> StoreResult<()> {
        if message.parse::<u64>().is_err() || channel.parse::<u64>().is_err() {
            return Err(StoreError::InvalidInput(
                "invalid Discord mirror cursor".into(),
            ));
        }
        self.with_conn(|c|c.execute("INSERT INTO imessage_owner_discord_cursors(revision,channel_id,message_id)
            SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM imessage_owner_binding WHERE enabled=1 AND revision=?1)
            ON CONFLICT(revision,channel_id) DO UPDATE SET message_id=excluded.message_id
            WHERE CAST(excluded.message_id AS INTEGER)>CAST(imessage_owner_discord_cursors.message_id AS INTEGER)",
            params![revision,channel,message]))?;
        Ok(())
    }
    /// Reserve the exact complete, delivered presentation before invoking an
    /// existing handler. Reference and reply target, when both present, must agree.
    pub fn lease_imessage_owner_action(
        &self,
        event: &OwnerEvent,
        reference: Option<&str>,
        reply: Option<&str>,
    ) -> StoreResult<String> {
        self.lease_imessage_owner_action_in_state(event, reference, reply, "pending")
    }
    pub fn lease_imessage_owner_action_in_state(
        &self,
        event: &OwnerEvent,
        reference: Option<&str>,
        reply: Option<&str>,
        status: &str,
    ) -> StoreResult<String> {
        if reference.is_none() && reply.is_none()
            || reference.is_some_and(|r| {
                r.len() < 6 || r.len() > 32 || !r.chars().all(|c| c.is_ascii_hexdigit())
            })
        {
            return Err(StoreError::InvalidInput(
                "a unique presented draft reference is required".into(),
            ));
        }
        self.with_conn(|c| {
            let tx=rusqlite::Transaction::new_unchecked(c,rusqlite::TransactionBehavior::Immediate)?;
            let b=binding(&tx)?.filter(|b|b.enabled && b.revision==event.binding_revision)
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let receipt:Option<String>=tx.query_row("SELECT payload FROM imessage_owner_events
                WHERE revision=?1 AND guid=?2 AND kind=?3 AND state='claimed'",
                params![b.revision,event.guid,event.kind],|r|r.get(0)).optional()?;
            if receipt.as_deref()!=Some(&serde_json::to_string(event).map_err(|_|rusqlite::Error::InvalidQuery)?) {
                return Err(rusqlite::Error::InvalidQuery)
            }
            let candidates={
                let mut stmt=tx.prepare("SELECT DISTINCT action_id,approval_digest FROM (
                    SELECT d.action_id,d.approval_digest,d.origin_key FROM imessage_owner_deliveries d
                    JOIN imessage_outbox o ON o.id=d.outbox_id WHERE d.revision=?1 AND d.action_id IS NOT NULL
                    AND (?2 IS NULL OR d.presentation_ref LIKE ?2 || '%')
                    AND (?3 IS NULL OR EXISTS(SELECT 1 FROM imessage_owner_deliveries x
                        JOIN imessage_outbox y ON y.id=x.outbox_id WHERE x.revision=d.revision
                        AND x.origin_key=d.origin_key AND y.message_guid=?3))
                    GROUP BY d.action_id,d.approval_digest,d.origin_key
                    HAVING COUNT(*)=MAX(d.total_parts) AND SUM(CASE WHEN o.status='sent'
                        AND o.message_guid IS NOT NULL THEN 1 ELSE 0 END)=COUNT(*))")?;
                let rows=stmt.query_map(params![b.revision,reference.map(str::to_ascii_lowercase),reply],
                    |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            if candidates.len()!=1 {return Err(rusqlite::Error::InvalidQuery)}
            let (action,digest)=&candidates[0];
            let inflight:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM approval_history WHERE action_id=?1 AND finished_at IS NULL)",
                [action],|r|r.get(0))?;
            if inflight {return Err(rusqlite::Error::InvalidQuery)}
            if snapshot(&tx,action)?.as_ref()!=Some(digest) {return Err(rusqlite::Error::InvalidQuery)}
            let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM actions WHERE id=?1 AND status=?2)",
                params![action,status],|r|r.get(0))?;
            if !pending {return Err(rusqlite::Error::InvalidQuery)}
            tx.execute("INSERT INTO imessage_owner_action_leases(action_id,token,digest,revision,expected_status)
                VALUES(?1,?2,?3,?4,?5)",params![action,event_token(event),digest,b.revision,status])?;
            tx.commit()?; Ok(action.clone())
        })
    }
    pub fn imessage_owner_card_ref(&self, action: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            let Some(b) = binding(c)?.filter(|b| b.enabled) else {
                return Ok(None);
            };
            Ok(snapshot(c, action)?.map(|d| card_reference(b.revision, action, &d)))
        })
    }
    pub fn imessage_approval_digest(&self, action: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| snapshot(c, action))
    }

    pub fn imessage_owner_cards_needing_status(&self) -> StoreResult<Vec<String>> {
        self.with_conn(|c| {
            let Some(b)=binding(c)?.filter(|b|b.enabled) else {return Ok(vec![])};
            let mut stmt=c.prepare("SELECT DISTINCT a.id FROM imessage_owner_deliveries d JOIN actions a ON a.id=d.action_id
                WHERE d.revision=?1 AND a.status!='pending' AND NOT EXISTS(SELECT 1 FROM imessage_owner_deliveries s
                WHERE s.revision=d.revision AND s.origin_key='state:' || a.id || ':' || a.status)
                ORDER BY a.id LIMIT 100")?;
            let rows=stmt.query_map([b.revision],|r|r.get(0))?;rows.collect()
        })
    }
    /// A status notification has no approval association; its guard only
    /// prevents a stale state caption from being queued after a newer state.
    pub fn enqueue_imessage_owner_state(
        &self,
        revision: i64,
        action: &str,
        status: &str,
        digest: &str,
        body: &str,
        now: i64,
    ) -> StoreResult<usize> {
        self.enqueue_imessage_owner_output_inner(
            revision,
            &format!("state:{action}:{status}"),
            body,
            None,
            Some((action, status, digest)),
            now,
        )
    }
    /// Atomically enqueue all parts. A repeated origin is either identical or
    /// refused, never a different message hidden under an idempotency key.
    pub fn enqueue_imessage_owner_output(
        &self,
        revision: i64,
        origin: &str,
        body: &str,
        card: Option<(&str, &str, &str)>,
        now: i64,
    ) -> StoreResult<usize> {
        self.enqueue_imessage_owner_output_inner(revision, origin, body, card, None, now)
    }
    fn enqueue_imessage_owner_output_inner(
        &self,
        revision: i64,
        origin: &str,
        body: &str,
        card: Option<(&str, &str, &str)>,
        guard: Option<(&str, &str, &str)>,
        now: i64,
    ) -> StoreResult<usize> {
        if origin.is_empty()
            || origin.len() > 256
            || body.trim().is_empty()
            || body.chars().count() > 256_000
        {
            return Err(StoreError::InvalidInput("invalid owner output".into()));
        }
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            let Some(b)=binding(&tx)?.filter(|b|b.enabled && b.revision==revision) else {return Ok(0)};
            if let Some((action,status,digest))=guard {
                let current:Option<String>=tx.query_row("SELECT status FROM actions WHERE id=?1",[action],|r|r.get(0)).optional()?;
                if current.as_deref()!=Some(status) || snapshot(&tx,action)?.as_deref()!=Some(digest) {return Ok(0)}
            }
            let content_hash=format!("{:x}",Sha256::digest(body.as_bytes()));
            let digest=if let Some((action,draft,expected_digest))=card {
                let current:Option<String>=tx.query_row("SELECT draftBody FROM actions WHERE id=?1
                    ",[action],|r|r.get(0)).optional()?;
                if current.as_deref()!=Some(draft) {return Err(rusqlite::Error::InvalidQuery)}
                let digest=snapshot(&tx,action)?;
                if digest.as_deref()!=Some(expected_digest) {return Err(rusqlite::Error::InvalidQuery)}
                digest
            } else {None};
            let prior:Option<(String,Option<String>,Option<String>)>=tx.query_row(
                "SELECT content_hash,action_id,approval_digest FROM imessage_owner_deliveries
                WHERE revision=?1 AND origin_key=?2 LIMIT 1",params![b.revision,origin],
                |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some(prior)=prior {
                if prior!=(content_hash,card.map(|c|c.0.to_string()),digest) {
                    return Err(rusqlite::Error::InvalidQuery)
                }
                return Ok(0)
            }
            let chars=body.chars().collect::<Vec<_>>(); let parts=chars.chunks(1980).collect::<Vec<_>>();
            for (i,part) in parts.iter().enumerate() {
                let chunk=if parts.len()>1 {format!("[{}/{}] {}",i+1,parts.len(),part.iter().collect::<String>())}
                    else {part.iter().collect::<String>()};
                tx.execute("INSERT INTO imessage_outbox(action_id,target,target_kind,service,body,created_at_ms)
                    VALUES(?1,?2,'chat_guid','iMessage',?3,?4)",
                    params![format!("owner-surface:{}",uuid::Uuid::new_v4()),b.chat_guid,chunk,now])?;
                tx.execute("INSERT INTO imessage_owner_deliveries VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![b.revision,origin,i as i64,parts.len() as i64,content_hash,
                    card.map(|c|c.0),digest,
                    card.zip(digest.as_deref()).map(|((a,_,_),d)|card_reference(b.revision,a,d)),
                    tx.last_insert_rowid()])?;
            }
            tx.commit()?; Ok(parts.len())
        })
    }

    /// Release self-chat only after every possible agent echo is accounted for.
    pub fn reconcile_imessage_owner_echoes(&self, now: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            let Some(b)=binding(&tx)?.filter(|b|b.enabled && b.self_chat) else {return Ok(0)};
            let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM imessage_outbox
                WHERE status IN ('queued','claimed','unknown') AND target IN (?2,?3)) OR EXISTS(SELECT 1 FROM imessage_owner_deliveries d
                JOIN imessage_outbox o ON o.id=d.outbox_id WHERE d.revision=?1
                AND o.status='sent' AND o.message_guid IS NULL)",
                params![b.revision,b.chat_guid,b.owner_handle],|r|r.get(0))?;
            if busy {return Ok(0)}
            let n=tx.execute("UPDATE imessage_owner_events SET state=CASE WHEN EXISTS(
                SELECT 1 FROM imessage_outbox o WHERE o.message_guid=imessage_owner_events.guid
                AND o.status='sent') THEN 'ignored' ELSE 'accepted' END,updated_at_ms=?2
                WHERE revision=?1 AND state='quarantined'",params![b.revision,now])?;
            tx.commit()?; Ok(n)
        })
    }

    pub fn imessage_owner_binding(&self) -> StoreResult<Option<OwnerBinding>> {
        self.with_conn(binding)
    }

    /// Owner configuration only; callers must not expose this to agent tools.
    /// Capture starts at a supplied current native watermark, never at history.
    pub fn bind_imessage_owner(
        &self,
        chat: &str,
        owner: &str,
        self_chat: bool,
        current_rowid: i64,
    ) -> StoreResult<OwnerBinding> {
        if chat.trim().is_empty()
            || owner.trim().is_empty()
            || chat.len() > 512
            || owner.len() > 512
            || current_rowid < 0
        {
            return Err(StoreError::InvalidInput("invalid owner binding".into()));
        }
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let old = binding(&tx)?;
            if let Some(ref b) = old {
                if b.enabled
                    && b.chat_guid == chat
                    && b.owner_handle == owner
                    && b.self_chat == self_chat
                {
                    return Ok(b.clone());
                }
                // Fail closed until dispatch/reconciliation settles all prior work.
                let busy: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM imessage_owner_events
                    WHERE revision=?1 AND state IN ('claimed','uncertain')) OR EXISTS(
                    SELECT 1 FROM imessage_owner_deliveries d JOIN imessage_outbox o ON o.id=d.outbox_id
                    WHERE d.revision=?1 AND o.status IN ('claimed','unknown'))",
                    [b.revision],
                    |r| r.get(0),
                )?;
                if busy {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
            let revision = old.map_or(1, |b| b.revision + 1);
            tx.execute("INSERT OR IGNORE INTO imessage_owner_control_chats VALUES(?1)",[chat])?;
            tx.execute("UPDATE imessage_outbox SET status='failed',error_detail='owner binding changed'
                WHERE status='queued' AND id IN (SELECT outbox_id FROM imessage_owner_deliveries)",[])?;
            tx.execute(
                "INSERT INTO imessage_owner_binding VALUES(1,?1,?2,?3,?4,1,?5,?6)
                ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,
                chat_guid=excluded.chat_guid,owner_handle=excluded.owner_handle,
                self_chat=excluded.self_chat,enabled=1,cursor=excluded.cursor,activated_at_ms=excluded.activated_at_ms",
                params![revision, chat, owner, self_chat, current_rowid,chrono::Utc::now().timestamp_millis()],
            )?;
            let result = binding(&tx)?.expect("binding inserted");
            tx.commit()?;
            Ok(result)
        })
    }

    pub fn disable_imessage_owner(&self) -> StoreResult<()> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "UPDATE imessage_owner_binding SET enabled=0 WHERE singleton=1",
                [],
            )?;
            tx.execute(
                "UPDATE imessage_outbox SET status='failed',error_detail='owner surface disabled'
                WHERE status='queued' AND id IN (SELECT outbox_id FROM imessage_owner_deliveries)",
                [],
            )?;
            tx.commit()
        })?;
        Ok(())
    }

    /// Accept a bounded native batch and its watermark atomically. A retry may
    /// repeat an identical batch; a changed payload under the same identity fails.
    pub fn accept_imessage_owner_events(
        &self,
        revision: i64,
        watermark: i64,
        events: &[OwnerEvent],
        now: i64,
    ) -> StoreResult<usize> {
        if events.len() > 100
            || events.iter().any(|e| {
                e.body.chars().count() > 8000
                    || e.guid.is_empty()
                    || e.guid.len() > 512
                    || e.rowid <= 0
                    || e.binding_revision != revision
                    || e.rowid > watermark
                    || e.created_at_ms > chrono::Utc::now().timestamp_millis() + 300_000
                    || !matches!(
                        e.kind.as_str(),
                        "text" | "like_add" | "like_remove" | "other"
                    )
            })
        {
            return Err(StoreError::InvalidInput("invalid owner event batch".into()));
        }
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let b = binding(&tx)?
                .filter(|b| b.enabled && b.revision == revision)
                .ok_or(rusqlite::Error::InvalidQuery)?;
            if watermark < b.cursor {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let mut count = 0;
            for e in events {
                if e.created_at_ms < b.activated_at_ms {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                if e.chat_guid != b.chat_guid
                    || e.sender != b.owner_handle
                    || e.from_me != b.self_chat
                {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                let payload =
                    serde_json::to_string(e).map_err(|_| rusqlite::Error::InvalidQuery)?;
                let prior: Option<Option<String>> = tx
                    .query_row(
                        "SELECT payload FROM imessage_owner_events
                    WHERE revision=?1 AND guid=?2 AND kind=?3",
                        params![revision, e.guid, e.kind],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(prior) = prior {
                    if prior.as_deref().is_some_and(|p| p != payload) {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    continue;
                }
                if e.rowid <= b.cursor {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                // A self-chat sender must first reconcile native sent GUIDs.
                let state = if b.self_chat && e.kind=="text" {
                    "quarantined"
                } else {
                    "accepted"
                };
                count += tx.execute(
                    "INSERT INTO imessage_owner_events(revision,guid,kind,rowid_native,payload,state,created_at_ms,updated_at_ms)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
                    params![revision, e.guid, e.kind, e.rowid, payload, state, now],
                )?;
            }
            tx.execute(
                "UPDATE imessage_owner_binding SET cursor=?1 WHERE singleton=1",
                [watermark],
            )?;
            tx.execute("INSERT INTO imessage_owner_health(revision,capture_at_ms) VALUES(?1,?2)
                ON CONFLICT(revision) DO UPDATE SET capture_at_ms=excluded.capture_at_ms,capture_error=NULL",params![revision,now])?;
            tx.commit()?;
            Ok(count)
        })
    }

    /// One conversation has one in-flight command. Uncertain work blocks later
    /// commands too: automatic restart never executes it a second time.
    pub fn claim_imessage_owner_event(&self, now: i64) -> StoreResult<Option<OwnerEvent>> {
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            let Some(b)=binding(&tx)?.filter(|b|b.enabled) else {return Ok(None)};
            let first:Option<(String,String,String,Option<String>)>=tx.query_row(
                "SELECT guid,kind,state,payload FROM imessage_owner_events WHERE revision=?1
                 AND is_control=0 AND state IN ('accepted','quarantined','claimed','uncertain') ORDER BY rowid_native,guid,kind LIMIT 1",
                [b.revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let Some((guid,kind,state,payload))=first else {return Ok(None)};
            if state!="accepted" {return Ok(None)};
            let event=serde_json::from_str(&payload.ok_or(rusqlite::Error::InvalidQuery)?)
                .map_err(|_|rusqlite::Error::InvalidQuery)?;
            tx.execute("UPDATE imessage_owner_events SET state='claimed',updated_at_ms=?4
                WHERE revision=?1 AND guid=?2 AND kind=?3",params![b.revision,guid,kind,now])?;
            tx.commit()?; Ok(Some(event))
        })
    }

    pub fn park_imessage_owner_event(&self, event: &OwnerEvent, now: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE imessage_owner_events SET state='uncertain',updated_at_ms=?4
            WHERE revision=?1 AND guid=?2 AND kind=?3 AND state='claimed'",
                params![event.binding_revision, event.guid, event.kind, now],
            )
        })? == 1)
    }
    pub fn finish_imessage_owner_event(&self, event: &OwnerEvent, now: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE imessage_owner_events SET state='completed',updated_at_ms=?4
            WHERE revision=?1 AND guid=?2 AND kind=?3 AND state='claimed'",
                params![event.binding_revision, event.guid, event.kind, now],
            )?;
            if changed == 1 {
                tx.execute(
                    "DELETE FROM imessage_owner_action_leases WHERE token=?1",
                    [event_token(event)],
                )?;
            }
            tx.commit()?;
            Ok(changed)
        })? == 1)
    }

    /// Call once during startup before workers run, not periodically.
    pub fn interrupt_imessage_owner_events(&self, now: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE imessage_owner_events SET state='uncertain',updated_at_ms=?1
            WHERE state='claimed'",
                [now],
            )
        })
    }

    pub fn compact_imessage_owner_event_bodies(&self, before: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            let count=tx.execute("UPDATE imessage_owner_events SET payload=NULL
                WHERE state IN ('completed','ignored') AND updated_at_ms<?1 AND payload IS NOT NULL",[before])?;
            tx.execute("UPDATE imessage_outbox SET body='[Owner output body expired after 30 days]'
                WHERE created_at_ms<?1 AND status IN ('sent','failed') AND body!='[Owner output body expired after 30 days]' AND id IN
                (SELECT outbox_id FROM imessage_owner_deliveries WHERE action_id IS NULL)", [before])?;
            tx.commit()?;Ok(count)
        })
    }
}
