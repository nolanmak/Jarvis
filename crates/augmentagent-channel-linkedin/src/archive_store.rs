//! Atomic page persistence. No network, LLM, broker, or send calls belong here.
use crate::archive::{Conversation, Message, Page};
use augmentagent_store::{
    rusqlite::{params, OptionalExtension},
    Store,
};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub cursor: Option<String>,
    pub complete: bool,
    pub revision: i64,
    pub retry_at_ms: i64,
}
#[derive(Debug, Serialize)]
pub struct Coverage {
    pub account: String,
    pub category: String,
    pub mailbox_complete: bool,
    pub conversations: i64,
    pub complete_threads: i64,
    pub messages: i64,
    pub owner_messages: i64,
    pub group_conversations: i64,
    pub oldest_source_ms: Option<i64>,
    pub newest_source_ms: Option<i64>,
    pub queued_index: i64,
    pub missing_index: i64,
    pub stale_index: i64,
    pub missing_fts: i64,
    pub complete: bool,
}

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
        if let Some(next)=&page.next {
            tx.execute("INSERT INTO linkedin_archive_cursors(account,scope,cursor) VALUES(?1,?2,?3)",params![account,format!("mailbox:{category}"),next])?;
        }
        for conv in &page.items {
            // New activity starts a fresh history reconciliation. A cursor from
            // the previous traversal must not poison this cycle's loop ledger.
            tx.execute("DELETE FROM linkedin_archive_cursors WHERE account=?1 AND scope=?2 AND EXISTS(SELECT 1 FROM linkedin_archive_conversations WHERE account=?1 AND urn=?3 AND latest_ms<?4)",params![account,format!("thread:{}",conv.urn),conv.urn,conv.last_activity_ms])?;
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
        if let Some(next)=&page.next{
            tx.execute("INSERT INTO linkedin_archive_cursors(account,scope,cursor) VALUES(?1,?2,?3)",params![account,format!("thread:{conversation}"),next])?;
        }
        let (title,group):(String,bool)=tx.query_row("SELECT title,group_chat FROM linkedin_archive_conversations WHERE account=?1 AND urn=?2",params![account,conversation],|r|Ok((r.get(0)?,r.get(1)?)))?;
        for m in &page.items{
            let existing:Option<Option<String>>=tx.query_row("SELECT accountEntityId FROM emails WHERE messageId=?1",[&m.urn],|r|r.get(0)).optional()?;
            // Preserve existing primary-account references; scope a collision.
            let stored_id=if existing.as_ref().is_none_or(|value|value.as_deref().is_some_and(|a|a==format!("linkedin:{account}")||a==account)){m.urn.clone()}else{format!("linkedin-archive:{account}:{}",m.urn)};
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
    Ok(store.with_conn(|c|{
        let mailbox_complete=c.query_row("SELECT complete FROM linkedin_archive_mailboxes WHERE account=?1 AND category=?2",params![account,category],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(false);
        let (conversations,complete_threads,group_conversations)=c.query_row("SELECT COUNT(*),COALESCE(SUM(c.complete),0),COALESCE(SUM(c.group_chat),0) FROM linkedin_archive_conversations c JOIN linkedin_archive_categories k ON k.account=c.account AND k.conversation=c.urn WHERE k.account=?1 AND k.category=?2",params![account,category],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let (messages,owner_messages,oldest_source_ms,newest_source_ms,queued_index)=c.query_row("SELECT COUNT(*),COALESCE(SUM(m.from_me),0),MIN(m.source_ms),MAX(m.source_ms),COUNT(q.message_id) FROM linkedin_archive_messages m JOIN linkedin_archive_categories k ON k.account=m.account AND k.conversation=m.conversation LEFT JOIN message_index_queue q ON q.message_id=m.stored_id WHERE k.account=?1 AND k.category=?2",params![account,category],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let (missing_index,stale_index,missing_fts):(i64,i64,i64)=c.query_row(
            "SELECT COALESCE(SUM(mi.message_id IS NULL),0),
                COALESCE(SUM(mi.message_id IS NOT NULL AND (mi.extractor_version<?3 OR mi.from_me<>m.from_me OR mi.ts_ms<>m.source_ms)),0),
                COALESCE(SUM(mi.message_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM message_fts f WHERE f.rowid=mi.rowid)),0)
             FROM linkedin_archive_messages m
             JOIN linkedin_archive_categories k ON k.account=m.account AND k.conversation=m.conversation
             LEFT JOIN message_index mi ON mi.message_id=m.stored_id
             WHERE k.account=?1 AND k.category=?2",
            params![account,category,augmentagent_messages::EXTRACTOR_VERSION],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(Coverage{account:account.into(),category:category.into(),mailbox_complete,conversations,complete_threads,messages,owner_messages,group_conversations,oldest_source_ms,newest_source_ms,queued_index,missing_index,stale_index,missing_fts,complete:mailbox_complete&&conversations==complete_threads&&queued_index==0&&missing_index==0&&stale_index==0&&missing_fts==0})
    })?)
}

/// Explicit reconciliation of mailbox heads, without erasing imported history.
pub fn refresh_mailbox(store: &Store, account: &str, category: &str) -> anyhow::Result<()> {
    store.with_conn(|c| {
        let tx=c.unchecked_transaction()?;
        tx.execute("INSERT INTO linkedin_archive_mailboxes(account,category) VALUES(?1,?2) ON CONFLICT(account,category) DO UPDATE SET cursor=NULL,complete=0,revision=revision+1,last_error=NULL",params![account,category])?;
        tx.execute("DELETE FROM linkedin_archive_cursors WHERE account=?1 AND scope=?2",params![account,format!("mailbox:{category}")])?;
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
