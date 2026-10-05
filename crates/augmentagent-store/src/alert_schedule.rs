//! Persistent Discord escalation, independent of the approval carousel (#1392).
use crate::{
    owner_alerts::{AlertState, OwnerAlert},
    Store, StoreResult,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

// Correlated with email alias e and the evaluation time parameter ?1. Only
// evidence that can appear in the assessor's seven-day context reopens work.
const RELEVANT_MEETING_EVIDENCE: &str = "(SELECT MAX(m.updated_at_ms)
    FROM owner_alert_meetings m,json_each(m.payload,'$.participants') p
    WHERE m.account_id=e.accountEntityId AND m.cancelled=0
    AND m.start_ms>?1 AND m.start_ms<=?1+604800000
    AND (lower(trim(e.fromEmail))=lower(p.value)
        OR instr(lower(e.fromEmail),'<'||lower(p.value)||'>')>0))";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertDetails {
    pub message_id: String,
    pub thread_id: Option<String>,
    pub account_id: Option<String>,
    pub subject: String,
    pub evidence: String,
    pub deadline_kind: String,
    pub meeting_start_ms: Option<i64>,
    #[serde(default)]
    pub meeting_url: Option<String>,
    pub reply_resolves: bool,
}

#[derive(Debug, Clone)]
pub struct AlertNotice {
    pub id: i64,
    pub alert: OwnerAlert,
    pub details: Option<AlertDetails>,
    pub triggers: i64,
    pub attempt: i64,
}

pub(crate) fn migrate(c: &Connection) -> StoreResult<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS owner_alert_details (
        alert_id TEXT PRIMARY KEY REFERENCES owner_alerts(id), payload TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS owner_alert_schedule (
        alert_id TEXT PRIMARY KEY REFERENCES owner_alerts(id), generation INTEGER NOT NULL DEFAULT 0,
        consumed INTEGER NOT NULL DEFAULT 0, notice_count INTEGER NOT NULL DEFAULT 0,
        followup_ms INTEGER NOT NULL DEFAULT 600000);
        CREATE TABLE IF NOT EXISTS owner_alert_notices (
        id INTEGER PRIMARY KEY AUTOINCREMENT, alert_id TEXT NOT NULL REFERENCES owner_alerts(id),
        generation INTEGER NOT NULL, triggers INTEGER NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('claimed','posted','failed','unknown','cancelled')),
        attempts INTEGER NOT NULL, claimed_at_ms INTEGER NOT NULL, next_attempt_ms INTEGER NOT NULL,
        posted_at_ms INTEGER, message_url TEXT, error TEXT);
        CREATE INDEX IF NOT EXISTS owner_alert_notices_status ON owner_alert_notices(status,next_attempt_ms);")?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS owner_alert_meetings (
        id TEXT PRIMARY KEY, account_id TEXT NOT NULL, start_ms INTEGER NOT NULL,
        payload TEXT NOT NULL, updated_at_ms INTEGER NOT NULL, cancelled INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS owner_alert_assessments (
        message_id TEXT PRIMARY KEY, assessed_at_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS owner_alert_assessment_failures (
        message_id TEXT PRIMARY KEY, attempts INTEGER NOT NULL,
        last_failed_at_ms INTEGER NOT NULL, next_attempt_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS owner_alert_priorities (
        sender TEXT PRIMARY KEY, urgency TEXT NOT NULL CHECK(urgency IN ('routine','high','critical')));
        CREATE TABLE IF NOT EXISTS owner_alert_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,alert_id TEXT NOT NULL,state TEXT NOT NULL,at_ms INTEGER NOT NULL);
        CREATE TRIGGER IF NOT EXISTS owner_alert_state_audit AFTER UPDATE OF state ON owner_alerts
        WHEN NEW.state!=OLD.state BEGIN INSERT INTO owner_alert_audit(alert_id,state,at_ms)
        VALUES(NEW.id,NEW.state,NEW.updated_at_ms); END;
        CREATE TABLE IF NOT EXISTS owner_alert_schedule_policy (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1), followup_ms INTEGER NOT NULL DEFAULT 600000,
        prepare_first_ms INTEGER NOT NULL DEFAULT 3600000,prepare_last_ms INTEGER NOT NULL DEFAULT 900000,
        max_notices INTEGER NOT NULL DEFAULT 6);
        INSERT OR IGNORE INTO owner_alert_schedule_policy(singleton) VALUES(1);")?;
    Ok(())
}

fn details(c: &Connection, id: &str) -> rusqlite::Result<Option<AlertDetails>> {
    let value: Option<String> = c
        .query_row(
            "SELECT payload FROM owner_alert_details WHERE alert_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()
}

fn notice(c: &Connection, id: i64) -> rusqlite::Result<AlertNotice> {
    let (alert_id, triggers, attempt): (String, i64, i64) = c.query_row(
        "SELECT alert_id,triggers,attempts FROM owner_alert_notices WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let alert = c.query_row(
        "SELECT * FROM owner_alerts WHERE id=?1",
        [&alert_id],
        crate::owner_alerts::row,
    )?;
    Ok(AlertNotice {
        id,
        alert,
        details: details(c, &alert_id)?,
        triggers,
        attempt,
    })
}

impl Store {
    pub fn tracked_owner_alert_meetings(&self, account: &str) -> StoreResult<Vec<String>> {
        self.with_conn(|c|{
            let mut q=c.prepare("SELECT m.id FROM owner_alert_meetings m WHERE m.account_id=?1 AND m.cancelled=0
                AND EXISTS(SELECT 1 FROM owner_alerts a WHERE a.meeting_id=m.id AND a.state!='resolved') ORDER BY m.start_ms LIMIT 100")?;
            let rows=q.query_map([account],|r|r.get(0))?;rows.collect()
        })
    }
    pub fn configure_owner_alert_schedule(
        &self,
        followup: i64,
        first: i64,
        last: i64,
        max: i64,
    ) -> StoreResult<()> {
        if followup < 60_000
            || first < last
            || last < 60_000
            || first > 604_800_000
            || !(1..=12).contains(&max)
        {
            return Err(crate::StoreError::InvalidInput("invalid reminder policy: positive minute-scale intervals, ordered preparation windows, and 1–12 notices required".into()));
        }
        self.with_conn(|c| {
            c.execute(
                "UPDATE owner_alert_schedule_policy SET followup_ms=?1,
            prepare_first_ms=?2,prepare_last_ms=?3,max_notices=?4 WHERE singleton=1",
                params![followup, first, last, max],
            )
        })?;
        Ok(())
    }
    pub fn set_owner_alert_priority(
        &self,
        sender: &str,
        urgency: crate::owner_alerts::Urgency,
    ) -> StoreResult<()> {
        if !sender.contains('@') || sender.contains(char::is_whitespace) {
            return Err(crate::StoreError::InvalidInput(
                "priority sender must be a bare email address".into(),
            ));
        }
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO owner_alert_priorities(sender,urgency) VALUES(lower(?1),?2)
            ON CONFLICT(sender) DO UPDATE SET urgency=excluded.urgency",
                params![sender, urgency.as_str()],
            )?;
            tx.execute(
                "DELETE FROM owner_alert_assessments WHERE message_id IN
                (SELECT messageId FROM emails WHERE lower(trim(fromEmail))=lower(?1)
                OR instr(lower(fromEmail),'<'||lower(?1)||'>')>0)",
                [sender],
            )?;
            tx.execute(
                "DELETE FROM owner_alert_assessment_failures WHERE message_id IN
                (SELECT messageId FROM emails WHERE lower(trim(fromEmail))=lower(?1)
                OR instr(lower(fromEmail),'<'||lower(?1)||'>')>0)",
                [sender],
            )?;
            tx.commit()
        })?;
        Ok(())
    }

    pub fn owner_alert_priority(
        &self,
        sender: &str,
    ) -> StoreResult<Option<crate::owner_alerts::Urgency>> {
        let value: Option<String> = self.with_conn(|c| {
            c.query_row(
                "SELECT urgency FROM owner_alert_priorities WHERE sender=lower(?1)",
                [sender],
                |r| r.get(0),
            )
            .optional()
        })?;
        Ok(value.map(|s| match s.as_str() {
            "critical" => crate::owner_alerts::Urgency::Critical,
            "high" => crate::owner_alerts::Urgency::High,
            _ => crate::owner_alerts::Urgency::Routine,
        }))
    }

    pub fn cache_owner_alert_meeting(
        &self,
        id: &str,
        account: &str,
        start: i64,
        payload: &str,
        now: i64,
    ) -> StoreResult<()> {
        self.with_conn(|c|c.execute("INSERT INTO owner_alert_meetings(id,account_id,start_ms,payload,updated_at_ms)
            VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET account_id=excluded.account_id,
            start_ms=excluded.start_ms,payload=excluded.payload,updated_at_ms=excluded.updated_at_ms,cancelled=0
            WHERE owner_alert_meetings.payload!=excluded.payload OR owner_alert_meetings.cancelled=1",params![id,account,start,payload,now]))?;
        self.update_owner_alert_meeting(id, Some(start), now)
    }

    pub fn cancel_owner_alert_meeting(&self, id: &str, now: i64) -> StoreResult<()> {
        self.with_conn(|c|c.execute("UPDATE owner_alert_meetings SET cancelled=1,updated_at_ms=?2 WHERE id=?1 AND cancelled=0",params![id,now]))?;
        self.update_owner_alert_meeting(id, None, now)
    }

    /// Revoke model context and reminders when a meeting becomes private.
    pub fn forget_owner_alert_meeting(&self, id: &str, now: i64) -> StoreResult<()> {
        // First exclude from retrieval and stop linked reminders. Retain no
        // cached summary or participants after the visibility change.
        self.cancel_owner_alert_meeting(id, now)?;
        self.with_conn(|c| c.execute("DELETE FROM owner_alert_meetings WHERE id=?1", [id]))?;
        Ok(())
    }

    pub fn owner_alert_meetings(&self, account: &str, now: i64) -> StoreResult<Vec<String>> {
        self.with_conn(|c| {
            let mut q = c.prepare(
                "SELECT payload FROM owner_alert_meetings WHERE account_id=?1 AND cancelled=0
                AND start_ms>?2 AND start_ms<=?2+604800000 ORDER BY start_ms LIMIT 100",
            )?;
            let rows = q.query_map(params![account, now], |r| r.get(0))?;
            rows.collect()
        })
    }

    pub fn mark_owner_alert_assessed(&self, message_id: &str, now: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO owner_alert_assessments(message_id,assessed_at_ms) VALUES(?1,?2)
                ON CONFLICT(message_id) DO UPDATE SET assessed_at_ms=excluded.assessed_at_ms",
                params![message_id, now],
            )?;
            tx.execute(
                "DELETE FROM owner_alert_assessment_failures WHERE message_id=?1",
                [message_id],
            )?;
            tx.commit()
        })?;
        Ok(())
    }

    /// Three attempts per evidence generation, with one- and five-minute delays.
    /// Failure metadata survives restarts without storing private model output.
    pub fn record_owner_alert_assessment_failure(
        &self,
        message_id: &str,
        now: i64,
    ) -> StoreResult<()> {
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let evidence: Option<i64> = tx.query_row(
                &format!("SELECT {RELEVANT_MEETING_EVIDENCE} FROM emails e WHERE e.messageId=?2"),
                params![now, message_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO owner_alert_assessment_failures
                (message_id,attempts,last_failed_at_ms,next_attempt_ms) VALUES(?1,1,?2,?2+60000)
                ON CONFLICT(message_id) DO UPDATE SET
                attempts=CASE WHEN ?3>last_failed_at_ms THEN 1 ELSE MIN(attempts+1,3) END,
                next_attempt_ms=?2+CASE WHEN ?3>last_failed_at_ms THEN 60000 ELSE 300000 END,
                last_failed_at_ms=?2",
                params![message_id, now, evidence],
            )?;
            tx.commit()
        })?;
        Ok(())
    }

    /// Previous-day/weekend messages remain eligible. New calendar evidence
    /// reopens assessment even when triage had already processed the email.
    pub fn owner_alert_backfill_candidates(
        &self,
        now: i64,
        limit: i64,
    ) -> StoreResult<Vec<crate::Email>> {
        self.with_conn(|c|{
            let mut q=c.prepare(&format!("SELECT e.* FROM emails e
                LEFT JOIN owner_alert_assessments x ON x.message_id=e.messageId
                LEFT JOIN owner_alert_assessment_failures f ON f.message_id=e.messageId
                WHERE e.platform='gmail' AND e.firstSeenAt>=?1-1209600000
                AND (e.triageResult IN ('reply','flag') OR EXISTS(SELECT 1 FROM owner_alert_priorities p
                    WHERE (lower(trim(e.fromEmail))=p.sender OR instr(lower(e.fromEmail),'<'||p.sender||'>')>0)
                    AND p.urgency!='routine'))
                AND (x.message_id IS NULL OR {RELEVANT_MEETING_EVIDENCE}>x.assessed_at_ms)
                AND (f.message_id IS NULL OR (f.attempts<3 AND f.next_attempt_ms<=?1)
                    OR {RELEVANT_MEETING_EVIDENCE}>f.last_failed_at_ms)
                AND NOT EXISTS(SELECT 1 FROM owner_alerts a WHERE a.id='gmail:'||e.messageId AND a.state='resolved')
                ORDER BY EXISTS(SELECT 1 FROM owner_alert_meetings m,json_each(m.payload,'$.participants') p
                    WHERE m.account_id=e.accountEntityId AND m.cancelled=0 AND m.start_ms>?1 AND m.start_ms<=?1+86400000
                    AND (lower(trim(e.fromEmail))=lower(p.value) OR instr(lower(e.fromEmail),'<'||lower(p.value)||'>')>0)) DESC,
                    e.firstSeenAt DESC LIMIT ?2"))?;
            let rows=q.query_map(params![now,limit],|r|Ok(crate::Email{
                message_id:r.get("messageId")?,thread_id:r.get("threadId")?,from:r.get("fromEmail")?,subject:r.get("subject")?,
                body:r.get::<_,Option<String>>("body")?.unwrap_or_default(),date:r.get::<_,Option<String>>("receivedAt")?.unwrap_or_default(),
                account_entity_id:r.get("accountEntityId")?,platform:r.get("platform")?,kind:r.get("kind")?,
                to:String::new(),cc:String::new(),attachments:Vec::new(),
            }))?;rows.collect()
        })
    }

    pub fn update_owner_alert_meeting(
        &self,
        meeting_id: &str,
        start: Option<i64>,
        now: i64,
    ) -> StoreResult<()> {
        self.with_conn(|c|{
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let ids={let mut q=tx.prepare("SELECT id FROM owner_alerts WHERE meeting_id=?1 AND state!='resolved'")?;
                let rows=q.query_map([meeting_id],|r|r.get::<_,String>(0))?;rows.collect::<rusqlite::Result<Vec<_>>>()?};
            for id in ids {
                let Some(mut d)=details(&tx,&id)? else{continue};
                if let Some(start)=start {
                    if d.meeting_start_ms==Some(start){continue;}
                    d.meeting_start_ms=Some(start);
                    let payload=serde_json::to_string(&d).map_err(|e|rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    tx.execute("UPDATE owner_alert_details SET payload=?2 WHERE alert_id=?1",params![id,payload])?;
                    if d.deadline_kind=="inferred_preparation" {
                        tx.execute("UPDATE owner_alerts SET due_at_ms=?2,expires_at_ms=?2,updated_at_ms=?3 WHERE id=?1",params![id,start,now])?;
                    }
                    tx.execute("UPDATE owner_alert_schedule SET generation=generation+1,consumed=consumed & 3 WHERE alert_id=?1",[&id])?;
                    tx.execute("UPDATE owner_alert_notices SET status='cancelled' WHERE alert_id=?1 AND status IN ('claimed','failed')",[&id])?;
                } else {
                    tx.execute("UPDATE owner_alerts SET state='resolved',updated_at_ms=?2 WHERE id=?1",params![id,now])?;
                }
            }
            tx.commit()
        })?;
        self.reconcile_owner_alert_texts(now)
    }

    pub fn attach_owner_alert_details(
        &self,
        id: &str,
        value: &AlertDetails,
        _now: i64,
    ) -> StoreResult<()> {
        let payload = serde_json::to_string(value)
            .map_err(|e| crate::StoreError::InvalidInput(e.to_string()))?;
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO owner_alert_details(alert_id,payload) VALUES(?1,?2)
            ON CONFLICT(alert_id) DO NOTHING",
                params![id, payload],
            )
        })?;
        Ok(())
    }

    /// Coalesces initial, unacknowledged follow-up, 60-minute and 15-minute
    /// preparation triggers into one post per evaluation. Resolution and expiry
    /// cancel all; acknowledgment cancels the follow-up but retains preparation.
    pub fn claim_owner_alert_notice(&self, now: i64) -> StoreResult<Option<AlertNotice>> {
        self.with_conn(|c|{
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            // Reuse verified external-reply evidence (#1275), scoped to the
            // account and only for tasks whose required action is a reply.
            // Compare with receipt time: the owner may reply before polling
            // ingests the email. Retain millisecond precision and fall back
            // to firstSeenAt when the source timestamp cannot be parsed.
            tx.execute("UPDATE owner_alerts SET state='resolved',updated_at_ms=?1
                WHERE state!='resolved' AND id IN (SELECT d.alert_id FROM owner_alert_details d
                JOIN emails e ON e.messageId=json_extract(d.payload,'$.message_id')
                JOIN outbound_thread_log o ON o.thread_id=json_extract(d.payload,'$.thread_id')
                AND o.entity_id=json_extract(d.payload,'$.account_id')
                WHERE json_extract(d.payload,'$.reply_resolves')=1
                AND o.sent_at_ms>COALESCE(CAST(round(unixepoch(e.receivedAt,'subsec')*1000) AS INTEGER),e.firstSeenAt))",[now])?;
            let (followup,first,last,max):(i64,i64,i64,i64)=tx.query_row("SELECT followup_ms,prepare_first_ms,prepare_last_ms,max_notices
                FROM owner_alert_schedule_policy WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            tx.execute("INSERT OR IGNORE INTO owner_alert_schedule(alert_id) SELECT id FROM owner_alerts WHERE urgency!='routine'",[])?;
            tx.execute("UPDATE owner_alert_notices SET status='cancelled' WHERE status IN ('failed','claimed')
                AND alert_id IN (SELECT id FROM owner_alerts WHERE state='resolved' OR expires_at_ms<=?1 OR due_at_ms<=?1
                OR (state='acknowledged' AND (owner_alert_notices.triggers & 12)=0))",[now])?;
            tx.execute("UPDATE owner_alert_notices SET status='unknown',error='Discord outcome unknown after interrupted posting'
                WHERE status='claimed' AND claimed_at_ms<=?1-120000",[now])?;
            tx.execute("UPDATE owner_alerts SET text_after_ms=MIN(text_after_ms,?1) WHERE state='open'
                AND id IN (SELECT alert_id FROM owner_alert_notices WHERE status='unknown')",[now])?;
            let retry:Option<i64>=tx.query_row("SELECT n.id FROM owner_alert_notices n JOIN owner_alerts a ON a.id=n.alert_id
                JOIN owner_alert_schedule s ON s.alert_id=a.id AND s.generation=n.generation
                WHERE n.status='failed' AND n.attempts<3 AND n.next_attempt_ms<=?1
                AND (a.snoozed_until_ms IS NULL OR a.snoozed_until_ms<=?1) ORDER BY n.id LIMIT 1",[now],|r|r.get(0)).optional()?;
            if let Some(id)=retry {
                tx.execute("UPDATE owner_alert_notices SET status='claimed',attempts=attempts+1,claimed_at_ms=?2 WHERE id=?1",params![id,now])?;
                let result=notice(&tx,id)?;tx.commit()?;return Ok(Some(result));
            }
            let candidates={
                let mut q=tx.prepare("SELECT a.* FROM owner_alerts a JOIN owner_alert_schedule s ON s.alert_id=a.id
                    WHERE a.state!='resolved' AND a.urgency!='routine' AND a.expires_at_ms>?1
                    AND (a.due_at_ms IS NULL OR a.due_at_ms>?1)
                    AND (a.snoozed_until_ms IS NULL OR a.snoozed_until_ms<=?1) AND s.notice_count<?2
                    AND NOT EXISTS (SELECT 1 FROM owner_alert_notices n WHERE n.alert_id=a.id
                        AND (n.status='claimed' OR (n.status='failed' AND n.attempts<3 AND n.generation=s.generation)))
                    ORDER BY CASE a.urgency WHEN 'critical' THEN 0 ELSE 1 END,a.created_at_ms LIMIT 500")?;
                let rows=q.query_map(params![now,max],crate::owner_alerts::row)?;rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for alert in candidates {
                let (consumed,generation):(i64,i64)=tx.query_row("SELECT consumed,generation FROM owner_alert_schedule WHERE alert_id=?1",[&alert.id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                let metadata=details(&tx,&alert.id)?;
                let mut due=0;
                if alert.state==AlertState::Open {
                    due|=1;
                    if now>=alert.created_at_ms.saturating_add(followup){due|=2;}
                }
                if let Some(start)=metadata.as_ref().and_then(|d|d.meeting_start_ms) {
                    if now>=start-first{due|=4;}
                    if now>=start-last{due|=8;}
                }
                let pending=due & !consumed;
                if pending==0{continue;}
                tx.execute("INSERT INTO owner_alert_notices(alert_id,generation,triggers,status,attempts,claimed_at_ms,next_attempt_ms)
                    VALUES(?1,?2,?3,'claimed',1,?4,?4)",params![alert.id,generation,pending,now])?;
                let id=tx.last_insert_rowid();
                tx.execute("UPDATE owner_alert_schedule SET consumed=consumed|?2,notice_count=notice_count+1 WHERE alert_id=?1",params![alert.id,due])?;
                tx.commit()?;
                return Ok(Some(AlertNotice{id,alert,details:metadata,triggers:pending,attempt:1}));
            }
            tx.commit()?;Ok(None)
        })
    }

    pub fn complete_owner_alert_notice(
        &self,
        id: i64,
        url: Option<&str>,
        error: Option<&str>,
        now: i64,
    ) -> StoreResult<()> {
        self.with_conn(|c|{
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let status=if url.is_some(){"posted"}else{"failed"};
            tx.execute("UPDATE owner_alert_notices SET status=?2,message_url=?3,error=?4,
                posted_at_ms=CASE WHEN ?2='posted' THEN ?5 ELSE NULL END,next_attempt_ms=?5+60000
                WHERE id=?1 AND status IN ('claimed','cancelled')",params![id,status,url,error,now])?;
            if url.is_none(){
                tx.execute("UPDATE owner_alerts SET text_after_ms=MIN(text_after_ms,?2)
                    WHERE id=(SELECT alert_id FROM owner_alert_notices WHERE id=?1) AND state='open'",params![id,now])?;
            }
            tx.commit()
        })
    }

    pub fn owner_alert_notice_status(&self, id: i64) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT status FROM owner_alert_notices WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
        })
    }
}
