//! Atomic page persistence. No network, LLM, broker, or send calls belong here.
use crate::archive::{Conversation, Message, Page};
use augmentagent_store::{
    rusqlite::{params, OptionalExtension},
    Store,
};

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub cursor: Option<String>,
    pub complete: bool,
    pub revision: i64,
    pub retry_at_ms: i64,
}
pub use augmentagent_store::linkedin_archive::Coverage;

pub fn checkpoint(store: &Store, account: &str, category: &str) -> anyhow::Result<Checkpoint> {
    Ok(store.with_conn(|c| {
        c.execute("INSERT OR IGNORE INTO linkedin_archive_mailboxes(account,category) VALUES(?1,?2)",params![account,category])?;
        c.query_row("SELECT cursor,complete,revision,retry_at_ms FROM linkedin_archive_mailboxes WHERE account=?1 AND category=?2", params![account,category], |r| Ok(Checkpoint {cursor:r.get(0)?, complete:r.get(1)?,revision:r.get(2)?,retry_at_ms:r.get(3)?}))
    })?)
}

/// Returns false if another importer committed the checkpoint first. The caller
/// reloads it instead of letting an older page rewind durable progress.
pub fn commit_conversations(
    store: &Store,
    account: &str,
    category: &str,
    expected: &Checkpoint,
    page: &Page<Conversation>,
    now: i64,
) -> anyhow::Result<bool> {
    Ok(store.with_conn(|c| {
        let tx=c.unchecked_transaction()?;
        let changed=tx.execute("UPDATE linkedin_archive_mailboxes SET cursor=?4,complete=?5,revision=revision+1,last_success_ms=?6,last_error=NULL,retry_at_ms=0 WHERE account=?1 AND category=?2 AND revision=?3 AND complete=0",params![account,category,expected.revision,page.next,page.next.is_none(),now])?;
        if changed==0 { return Ok(false); }
        count_page(&tx, account, &format!("mailbox:{category}"))?;
        if let Some(next)=&page.next {
            tx.execute("INSERT INTO linkedin_archive_cursors(account,scope,cursor) VALUES(?1,?2,?3)",params![account,format!("mailbox:{category}"),next])?;
        }
        for conv in &page.items {
            // New activity starts a fresh history reconciliation. A cursor from
            // the previous traversal must not poison this cycle's loop ledger.
            tx.execute("DELETE FROM linkedin_archive_cursors WHERE account=?1 AND scope=?2 AND EXISTS(SELECT 1 FROM linkedin_archive_conversations WHERE account=?1 AND urn=?3 AND latest_ms<?4)",params![account,format!("thread:{}",conv.urn),conv.urn,conv.last_activity_ms])?;
            tx.execute("DELETE FROM linkedin_archive_restarts WHERE account=?1 AND scope=?2 AND EXISTS(SELECT 1 FROM linkedin_archive_conversations WHERE account=?1 AND urn=?3 AND latest_ms<?4)",params![account,format!("thread:{}",conv.urn),conv.urn,conv.last_activity_ms])?;
            tx.execute("UPDATE linkedin_archive_page_stats SET current_cycle=0 WHERE account=?1 AND scope=?2 AND EXISTS(SELECT 1 FROM linkedin_archive_conversations WHERE account=?1 AND urn=?3 AND latest_ms<?4)",params![account,format!("thread:{}",conv.urn),conv.urn,conv.last_activity_ms])?;
            tx.execute("INSERT INTO linkedin_archive_conversations(account,urn,title,group_chat,latest_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(account,urn) DO UPDATE SET title=excluded.title,group_chat=excluded.group_chat,latest_ms=MAX(latest_ms,excluded.latest_ms),complete=CASE WHEN excluded.latest_ms>latest_ms THEN 0 ELSE complete END,cursor=CASE WHEN excluded.latest_ms>latest_ms THEN NULL ELSE cursor END,revision=revision+1",params![account,conv.urn,conv.title,conv.group,conv.last_activity_ms])?;
            tx.execute("INSERT OR IGNORE INTO linkedin_archive_categories(account,category,conversation) VALUES(?1,?2,?3)",params![account,category,conv.urn])?;
            // Membership is the latest observed snapshot, not invented history.
            tx.execute("DELETE FROM linkedin_archive_participants WHERE account=?1 AND conversation=?2",params![account,conv.urn])?;
            for p in &conv.participants {
                tx.execute("INSERT INTO linkedin_archive_participants(account,conversation,urn,name,observed_at_ms) VALUES(?1,?2,?3,?4,?5)",params![account,conv.urn,p.urn,p.name,now])?;
            }
        }
        tx.commit()?;
        Ok(true)
    })?)
}

pub fn pending_threads(
    store: &Store,
    account: &str,
    category: &str,
) -> anyhow::Result<Vec<(String, Checkpoint)>> {
    Ok(store.with_conn(|c| {
        let mut q=c.prepare("SELECT c.urn,c.cursor,c.complete,c.revision,c.retry_at_ms FROM linkedin_archive_conversations c JOIN linkedin_archive_categories k ON k.account=c.account AND k.conversation=c.urn WHERE k.account=?1 AND k.category=?2 AND c.complete=0 ORDER BY c.latest_ms DESC,c.urn")?;
        let rows=q.query_map(params![account,category],|r|Ok((r.get(0)?,Checkpoint{cursor:r.get(1)?,complete:r.get(2)?,revision:r.get(3)?,retry_at_ms:r.get(4)?})))?.collect();
        rows
    })?)
}

pub fn commit_messages(
    store: &Store,
    account: &str,
    conversation: &str,
    expected: &Checkpoint,
    page: &Page<Message>,
    now: i64,
) -> anyhow::Result<bool> {
    // Fail before starting any transaction if transport confused thread scope.
    anyhow::ensure!(
        page.items.iter().all(|m| m.conversation == conversation),
        "message belongs to another conversation"
    );
    Ok(store.with_conn(|c|{
        let tx=c.unchecked_transaction()?;
        let changed=tx.execute("UPDATE linkedin_archive_conversations SET cursor=?4,complete=?5,revision=revision+1,last_success_ms=?6,last_error=NULL,retry_at_ms=0 WHERE account=?1 AND urn=?2 AND revision=?3 AND complete=0",params![account,conversation,expected.revision,page.next,page.next.is_none(),now])?;
        if changed==0{return Ok(false);}
        count_page(&tx, account, &format!("thread:{conversation}"))?;
        if let Some(next)=&page.next{
            tx.execute("INSERT INTO linkedin_archive_cursors(account,scope,cursor) VALUES(?1,?2,?3)",params![account,format!("thread:{conversation}"),next])?;
        }
        let (title,group):(String,bool)=tx.query_row("SELECT title,group_chat FROM linkedin_archive_conversations WHERE account=?1 AND urn=?2",params![account,conversation],|r|Ok((r.get(0)?,r.get(1)?)))?;
        for m in &page.items{
            let archived: Option<(String, String)> = tx.query_row(
                "SELECT conversation,stored_id FROM linkedin_archive_messages WHERE account=?1 AND urn=?2",
                params![account,m.urn], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional()?;
            if archived.as_ref().is_some_and(|(thread, _)| thread != conversation) {
                return Err(augmentagent_store::rusqlite::Error::InvalidParameterName("message identity belongs to another conversation".into()));
            }
            let existing:Option<Option<String>>=tx.query_row("SELECT accountEntityId FROM emails WHERE messageId=?1",[&m.urn],|r|r.get(0)).optional()?;
            // Preserve existing primary-account references; scope a collision.
            let stored_id=if let Some((_, id)) = archived { id } else if existing.as_ref().is_none_or(|value|value.as_deref().is_some_and(|a|a==format!("linkedin:{account}")||a==account)){m.urn.clone()}else{format!("linkedin-archive:{account}:{}",m.urn)};
            // Validate the chosen key too: a scoped fallback can itself collide
            // with legacy data. Never overwrite another account/platform/thread.
            let target: Option<(Option<String>, String, String)> = tx.query_row(
                "SELECT accountEntityId,platform,threadId FROM emails WHERE messageId=?1",
                [&stored_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
            ).optional()?;
            if target.is_some_and(|(owner, platform, thread)|
                !owner.as_deref().is_some_and(|a| a == account || a == format!("linkedin:{account}"))
                || platform != "linkedin" || thread != conversation
            ) {
                return Err(augmentagent_store::rusqlite::Error::InvalidParameterName("stored message identity conflicts with archive scope".into()));
            }
            let name:String=tx.query_row("SELECT name FROM linkedin_archive_participants WHERE account=?1 AND conversation=?2 AND urn=?3",params![account,conversation,m.sender],|r|r.get(0)).optional()?.unwrap_or_else(||m.sender.clone());
            let sender=format!("{name} <linkedin:{}>",m.sender);
            let date=time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(m.delivered_at_ms)*1_000_000).ok().and_then(|d|d.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
            let body=augmentagent_store::redact::mask(&m.body);
            // Separate insert/update preserves the existing queue trigger's
            // conflict policy. SQLite UPSERT's DO UPDATE can override a nested
            // trigger's OR REPLACE and fail when a message is already queued.
            tx.execute("INSERT OR IGNORE INTO emails(messageId,threadId,fromEmail,subject,body,receivedAt,accountEntityId,firstSeenAt,platform,kind,agentProcessedAt) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'linkedin',?9,?8)",params![stored_id,conversation,sender,format!("[LinkedIn DM from {title}]"),body.as_ref(),date,format!("linkedin:{account}"),now,if group{"group"}else{"dm"}])?;
            tx.execute("UPDATE emails SET threadId=?2,fromEmail=?3,subject=?4,body=?5,receivedAt=?6,accountEntityId=?7,kind=?8 WHERE messageId=?1",params![stored_id,conversation,sender,format!("[LinkedIn DM from {title}]"),body.as_ref(),date,format!("linkedin:{account}"),if group{"group"}else{"dm"}])?;
            let content=augmentagent_store::redact::mask(&m.content.to_string()).to_string();
            tx.execute("INSERT INTO linkedin_archive_messages(account,urn,conversation,stored_id,sender,from_me,source_ms,kind,content_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(account,urn) DO UPDATE SET sender=excluded.sender,from_me=excluded.from_me,source_ms=excluded.source_ms,kind=excluded.kind,content_json=excluded.content_json",params![account,m.urn,conversation,stored_id,m.sender,m.from_me,m.delivered_at_ms,m.kind,content])?;
            tx.execute("INSERT INTO message_index_queue(message_id) VALUES(?1) ON CONFLICT(message_id) DO NOTHING",[&stored_id])?;
        }
        tx.commit()?;
        Ok(true)
    })?)
}

pub fn coverage(store: &Store, account: &str, category: &str) -> anyhow::Result<Coverage> {
    Ok(store.with_conn(|c| {
        augmentagent_store::linkedin_archive::coverage(
            c,
            account,
            category,
            augmentagent_messages::EXTRACTOR_VERSION,
        )
    })?)
}

/// Explicit reconciliation of mailbox heads, without erasing imported history.
pub fn refresh_mailbox(store: &Store, account: &str, category: &str) -> anyhow::Result<()> {
    store.with_conn(|c| {
        let tx=c.unchecked_transaction()?;
        tx.execute("INSERT INTO linkedin_archive_mailboxes(account,category) VALUES(?1,?2) ON CONFLICT(account,category) DO UPDATE SET cursor=NULL,complete=0,revision=revision+1,last_error=NULL",params![account,category])?;
        tx.execute("DELETE FROM linkedin_archive_cursors WHERE account=?1 AND scope=?2",params![account,format!("mailbox:{category}")])?;
        tx.execute("DELETE FROM linkedin_archive_restarts WHERE account=?1 AND scope=?2",params![account,format!("mailbox:{category}")])?;
        tx.execute("UPDATE linkedin_archive_page_stats SET current_cycle=0 WHERE account=?1 AND scope=?2",params![account,format!("mailbox:{category}")])?;
        tx.commit()
    })?;
    Ok(())
}

pub fn record_failure(
    store: &Store,
    account: &str,
    scope: &str,
    thread: bool,
    error: &str,
    retry_at_ms: i64,
) -> anyhow::Result<()> {
    store.with_conn(|c| {
        let sql=if thread {"UPDATE linkedin_archive_conversations SET last_error=?3,retry_at_ms=?4 WHERE account=?1 AND urn=?2"}
            else {"UPDATE linkedin_archive_mailboxes SET last_error=?3,retry_at_ms=?4 WHERE account=?1 AND category=?2"};
        c.execute(sql,params![account,scope,error,retry_at_ms])?;Ok(())
    })?;
    Ok(())
}

/// Restart a rejected continuation once per reconciliation. The optimistic
/// checkpoint check keeps an older request from rewinding another importer's
/// progress. Existing messages and membership are retained.
pub fn restart_cursor(
    store: &Store,
    account: &str,
    scope: &str,
    thread: bool,
    expected: &Checkpoint,
    now: i64,
) -> anyhow::Result<bool> {
    if expected.cursor.is_none() {
        return Ok(false);
    }
    Ok(store.with_conn(|c| {
        let tx = c.unchecked_transaction()?;
        let sql = if thread {
            "UPDATE linkedin_archive_conversations SET cursor=NULL,revision=revision+1,last_error='cursor rejected; reconciliation restarted',retry_at_ms=0 WHERE account=?1 AND urn=?2 AND revision=?3 AND complete=0"
        } else {
            "UPDATE linkedin_archive_mailboxes SET cursor=NULL,revision=revision+1,last_error='cursor rejected; reconciliation restarted',retry_at_ms=0 WHERE account=?1 AND category=?2 AND revision=?3 AND complete=0"
        };
        if tx.execute(sql, params![account, scope, expected.revision])? == 0 {
            // A concurrent writer already changed progress: reload it.
            return Ok(true);
        }
        let ledger_scope = format!("{}:{scope}", if thread { "thread" } else { "mailbox" });
        if tx.execute("INSERT OR IGNORE INTO linkedin_archive_restarts(account,scope,restarted_at_ms) VALUES(?1,?2,?3)", params![account,ledger_scope,now])? == 0 {
            // Dropping the transaction also rolls back the cursor reset.
            return Ok(false);
        }
        tx.execute("DELETE FROM linkedin_archive_cursors WHERE account=?1 AND scope=?2", params![account,ledger_scope])?;
        tx.commit()?;
        Ok(true)
    })?)
}

pub fn account_retry_at(store: &Store, account: &str) -> anyhow::Result<i64> {
    Ok(store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT retry_at_ms FROM linkedin_archive_accounts WHERE account=?1",
            [account],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
    })?)
}

pub fn record_account_failure(
    store: &Store,
    account: &str,
    error: &str,
    retry_at_ms: i64,
) -> anyhow::Result<()> {
    store.with_conn(|c| {
        c.execute("INSERT INTO linkedin_archive_accounts(account,last_error,retry_at_ms) VALUES(?1,?2,?3) ON CONFLICT(account) DO UPDATE SET last_error=CASE WHEN excluded.retry_at_ms>=retry_at_ms THEN excluded.last_error ELSE last_error END,retry_at_ms=MAX(retry_at_ms,excluded.retry_at_ms)",params![account,error,retry_at_ms])?;
        Ok(())
    })?;
    Ok(())
}

/// A successful request may clear an expired failure, but cannot erase a newer
/// throttle concurrently observed by another importer.
pub fn clear_account_failure(
    store: &Store,
    account: &str,
    request_started_ms: i64,
) -> anyhow::Result<()> {
    store.with_conn(|c| {
        c.execute("UPDATE linkedin_archive_accounts SET last_error=NULL,retry_at_ms=0 WHERE account=?1 AND retry_at_ms<=?2",params![account,request_started_ms])?;
        Ok(())
    })?;
    Ok(())
}

fn count_page(
    tx: &augmentagent_store::rusqlite::Transaction<'_>,
    account: &str,
    scope: &str,
) -> augmentagent_store::rusqlite::Result<()> {
    tx.execute("INSERT INTO linkedin_archive_page_stats(account,scope,committed,current_cycle) VALUES(?1,?2,1,1) ON CONFLICT(account,scope) DO UPDATE SET committed=committed+1,current_cycle=current_cycle+1", params![account,scope])?;
    Ok(())
}
