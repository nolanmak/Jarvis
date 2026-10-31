//! Read-only LinkedIn archive coverage shared by CLI and search callers.
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coverage {
    pub account: String,
    pub category: String,
    pub mailbox_complete: bool,
    pub last_success_ms: Option<i64>,
    pub last_error: Option<String>,
    pub retry_at_ms: i64,
    pub failed_threads: i64,
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

pub fn coverage(
    c: &Connection,
    account: &str,
    category: &str,
    extractor_version: i64,
) -> rusqlite::Result<Coverage> {
    let (mailbox_complete,last_success_ms,last_error,retry_at_ms)=c.query_row("SELECT complete,last_success_ms,last_error,retry_at_ms FROM linkedin_archive_mailboxes WHERE account=?1 AND category=?2",params![account,category],|r|Ok((r.get::<_,bool>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,i64>(3)?))).optional()?.unwrap_or((false,None,None,0));
    let failed_threads:i64=c.query_row("SELECT COUNT(*) FROM linkedin_archive_conversations c JOIN linkedin_archive_categories k ON k.account=c.account AND k.conversation=c.urn WHERE k.account=?1 AND k.category=?2 AND c.last_error IS NOT NULL",params![account,category],|r|r.get(0))?;
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
            params![account,category,extractor_version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    let no_errors = last_error.is_none() && failed_threads == 0;
    Ok(Coverage {
        account: account.into(),
        category: category.into(),
        mailbox_complete,
        last_success_ms,
        last_error,
        retry_at_ms,
        failed_threads,
        conversations,
        complete_threads,
        messages,
        owner_messages,
        group_conversations,
        oldest_source_ms,
        newest_source_ms,
        queued_index,
        missing_index,
        stale_index,
        missing_fts,
        complete: no_errors
            && mailbox_complete
            && conversations == complete_threads
            && queued_index == 0
            && missing_index == 0
            && stale_index == 0
            && missing_fts == 0,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchCoverage {
    pub scope: &'static str,
    pub state: &'static str,
    pub scopes: Vec<Coverage>,
}
/// This is deliberately scoped to recorded categories, never a claim that an
/// unobserved mailbox category is empty or that a past observation is live.
pub fn search_coverage(c: &Connection, extractor_version: i64) -> rusqlite::Result<SearchCoverage> {
    let scopes = c
        .prepare(
            "SELECT account,category FROM linkedin_archive_mailboxes ORDER BY account,category",
        )?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let scopes = scopes
        .into_iter()
        .map(|(account, category)| coverage(c, &account, &category, extractor_version))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SearchCoverage {
        scope: "known_categories_only",
        state: if scopes.is_empty() {
            "not_synced"
        } else {
            "observed_scopes"
        },
        scopes,
    })
}
