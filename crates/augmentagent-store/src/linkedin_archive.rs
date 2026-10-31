//! Read-only LinkedIn archive coverage shared by CLI and search callers.
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountStatus {
    pub account: String,
    pub last_error: Option<String>,
    pub retry_at_ms: i64,
}

pub fn account_status(c: &Connection, account: &str) -> rusqlite::Result<AccountStatus> {
    let (last_error, retry_at_ms) = c
        .query_row(
            "SELECT last_error,retry_at_ms FROM linkedin_archive_accounts WHERE account=?1",
            [account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((None, 0));
    Ok(AccountStatus {
        account: account.into(),
        last_error,
        retry_at_ms,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageProgress {
    /// None means no durable page receipt has been recorded for this scope.
    pub committed: Option<i64>,
    pub current_cycle: Option<i64>,
}

pub fn page_progress(c: &Connection, account: &str, scope: &str) -> rusqlite::Result<PageProgress> {
    let (committed, current_cycle) = c.query_row(
        "SELECT committed,current_cycle FROM linkedin_archive_page_stats WHERE account=?1 AND scope=?2",
        params![account,scope], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?.unwrap_or((None,None));
    Ok(PageProgress {
        committed,
        current_cycle,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coverage {
    pub account: String,
    pub category: String,
    pub scope: &'static str,
    pub mailbox_pages: PageProgress,
    pub mailbox_has_continuation: bool,
    pub mailbox_complete: bool,
    pub account_status: AccountStatus,
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
    let account_status = account_status(c, account)?;
    let no_errors =
        last_error.is_none() && failed_threads == 0 && account_status.last_error.is_none();
    Ok(Coverage {
        account: account.into(),
        category: category.into(),
        scope: "all_accessible_history",
        mailbox_pages: page_progress(c, account, &format!("mailbox:{category}"))?,
        mailbox_has_continuation: c.query_row("SELECT cursor IS NOT NULL FROM linkedin_archive_mailboxes WHERE account=?1 AND category=?2", params![account,category], |r| r.get(0)).optional()?.unwrap_or(false),
        mailbox_complete,
        account_status,
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
    pub accounts: Vec<AccountStatus>,
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
    let accounts = c
        .prepare("SELECT account FROM linkedin_archive_accounts ORDER BY account")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|account| account_status(c, &account))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SearchCoverage {
        scope: "known_categories_only",
        state: if scopes.is_empty() {
            "not_synced"
        } else {
            "observed_scopes"
        },
        scopes,
        accounts,
    })
}

/// Detailed status is returned by the explicit status command, not attached to
/// every search result. No message bodies or opaque provider cursors are exposed.
#[derive(Debug, Serialize)]
pub struct ThreadStatus {
    pub account_id: String,
    pub conversation_id: String,
    pub kind: &'static str,
    pub history_state: &'static str,
    pub last_success_ms: Option<i64>,
    pub last_error: Option<String>,
    pub retry_at_ms: i64,
    pub latest_activity_ms: i64,
    pub has_continuation: bool,
    pub revision: i64,
    pub pages: PageProgress,
    pub messages: i64,
    pub owner_messages: i64,
    pub inbound_messages: i64,
    pub non_text_messages: i64,
    pub oldest_source_ms: Option<i64>,
    pub newest_source_ms: Option<i64>,
    pub queued_index: i64,
    pub missing_index: i64,
    pub stale_index: i64,
    pub missing_fts: i64,
}

pub fn thread_status(
    c: &Connection,
    category: &str,
    extractor_version: i64,
) -> rusqlite::Result<Vec<ThreadStatus>> {
    let mut q = c.prepare(
        "SELECT t.account,t.urn,t.group_chat,t.complete,t.last_success_ms,t.last_error,t.retry_at_ms,t.latest_ms,t.cursor IS NOT NULL,t.revision,
         p.committed,p.current_cycle,COUNT(m.urn),COALESCE(SUM(m.from_me),0),COALESCE(SUM(m.kind<>'text'),0),MIN(m.source_ms),MAX(m.source_ms),COUNT(iq.message_id),
         COALESCE(SUM(m.urn IS NOT NULL AND mi.message_id IS NULL),0),
         COALESCE(SUM(mi.message_id IS NOT NULL AND (mi.extractor_version<?2 OR mi.from_me<>m.from_me OR mi.ts_ms<>m.source_ms)),0),
         COALESCE(SUM(mi.message_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM message_fts f WHERE f.rowid=mi.rowid)),0)
         FROM linkedin_archive_conversations t
         JOIN linkedin_archive_categories k ON k.account=t.account AND k.conversation=t.urn
         LEFT JOIN linkedin_archive_page_stats p ON p.account=t.account AND p.scope='thread:'||t.urn
         LEFT JOIN linkedin_archive_messages m ON m.account=t.account AND m.conversation=t.urn
         LEFT JOIN message_index_queue iq ON iq.message_id=m.stored_id
         LEFT JOIN message_index mi ON mi.message_id=m.stored_id
         WHERE k.category=?1 GROUP BY t.account,t.urn ORDER BY t.account,t.urn"
    )?;
    let rows = q
        .query_map(params![category, extractor_version], |r| {
            let complete: bool = r.get(3)?;
            let last_success_ms: Option<i64> = r.get(4)?;
            let last_error: Option<String> = r.get(5)?;
            let messages: i64 = r.get(12)?;
            let owner_messages: i64 = r.get(13)?;
            Ok(ThreadStatus {
                account_id: format!("linkedin:{}", r.get::<_, String>(0)?),
                conversation_id: r.get(1)?,
                kind: if r.get::<_, bool>(2)? {
                    "group"
                } else {
                    "direct"
                },
                history_state: if last_error.is_some() {
                    "failed"
                } else if complete {
                    "complete_at_observation"
                } else if last_success_ms.is_some() {
                    "partial"
                } else {
                    "unknown"
                },
                last_success_ms,
                last_error,
                retry_at_ms: r.get(6)?,
                latest_activity_ms: r.get(7)?,
                has_continuation: r.get(8)?,
                revision: r.get(9)?,
                pages: PageProgress {
                    committed: r.get(10)?,
                    current_cycle: r.get(11)?,
                },
                messages,
                owner_messages,
                inbound_messages: messages - owner_messages,
                non_text_messages: r.get(14)?,
                oldest_source_ms: r.get(15)?,
                newest_source_ms: r.get(16)?,
                queued_index: r.get(17)?,
                missing_index: r.get(18)?,
                stale_index: r.get(19)?,
                missing_fts: r.get(20)?,
            })
        })?
        .collect();
    rows
}
