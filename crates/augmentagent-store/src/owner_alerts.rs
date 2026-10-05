//! Durable owner-alert contract shared by Discord and iMessage (#1392, #1393).
//! Acknowledgment is visibility, never approval to send a reply or task completion.
use crate::{Store, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Routine,
    High,
    Critical,
}
impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Routine => "routine",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertState {
    Open,
    Acknowledged,
    Resolved,
}
impl AlertState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Acknowledged => "acknowledged",
            Self::Resolved => "resolved",
        }
    }
}

pub struct NewOwnerAlert<'a> {
    /// Stable source-derived ID; duplicate ingestion must reuse it.
    pub id: &'a str,
    pub source_url: &'a str,
    pub sender: &'a str,
    pub action: &'a str,
    pub reason: &'a str,
    pub urgency: Urgency,
    pub due_at_ms: Option<i64>,
    pub timezone: &'a str,
    pub meeting_id: Option<&'a str>,
    /// Usefulness cutoff, including for alerts with unknown deadlines.
    pub expires_at_ms: i64,
    pub text_after_ms: i64,
}

#[derive(Debug, Clone)]
pub struct OwnerAlert {
    pub id: String,
    pub source_url: String,
    pub sender: String,
    pub action: String,
    pub reason: String,
    pub urgency: Urgency,
    pub due_at_ms: Option<i64>,
    pub timezone: String,
    pub meeting_id: Option<String>,
    pub expires_at_ms: i64,
    pub text_after_ms: i64,
    pub state: AlertState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub snoozed_until_ms: Option<i64>,
}

#[derive(Debug)]
pub struct OwnerTextHealth {
    pub enabled: bool,
    pub healthy: bool,
    pub detail: String,
    pub heartbeat_at_ms: Option<i64>,
    pub last_success_at_ms: Option<i64>,
}

impl OwnerAlert {
    pub fn text_summary(&self, now: i64) -> String {
        let deadline = self
            .due_at_ms
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|due| {
                let zone: chrono_tz::Tz = self.timezone.parse().unwrap_or(chrono_tz::UTC);
                format!(
                    "{} (in {} min)",
                    due.with_timezone(&zone).format("%b %d %H:%M %Z"),
                    (due.timestamp_millis().saturating_sub(now).max(0) + 59_999) / 60_000
                )
            })
            .unwrap_or_else(|| "unknown".into());
        format!(
            "{}: {}\nAction: {}\nDue: {}\nWhy: {}\n{}",
            self.urgency.as_str().to_uppercase(),
            self.sender,
            self.action,
            deadline,
            self.reason,
            self.source_url
        )
    }
}

pub(crate) fn migrate(c: &Connection) -> StoreResult<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner_alerts (
        id TEXT PRIMARY KEY, source_url TEXT NOT NULL, sender TEXT NOT NULL,
        action TEXT NOT NULL, reason TEXT NOT NULL,
        urgency TEXT NOT NULL CHECK(urgency IN ('routine','high','critical')),
        due_at_ms INTEGER, timezone TEXT NOT NULL, meeting_id TEXT,
        expires_at_ms INTEGER NOT NULL, text_after_ms INTEGER NOT NULL,
        state TEXT NOT NULL DEFAULT 'open' CHECK(state IN ('open','acknowledged','resolved')),
        created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
        snoozed_until_ms INTEGER);
        CREATE INDEX IF NOT EXISTS owner_alerts_text_due ON owner_alerts(state, text_after_ms);
        CREATE TABLE IF NOT EXISTS owner_alert_text_policy (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), destination TEXT,
            enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0,1)));
        INSERT OR IGNORE INTO owner_alert_text_policy(singleton) VALUES(1);
        CREATE TABLE IF NOT EXISTS owner_alert_texts (
            alert_id TEXT PRIMARY KEY REFERENCES owner_alerts(id),
            outbox_id INTEGER NOT NULL UNIQUE REFERENCES imessage_outbox(id),
            disposition TEXT CHECK(disposition IN ('cancelled','expired')));
        CREATE TABLE IF NOT EXISTS owner_text_sender_health (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), heartbeat_at_ms INTEGER,
            error TEXT, notified_at_ms INTEGER);
        INSERT OR IGNORE INTO owner_text_sender_health(singleton) VALUES(1);",
    )?;
    Ok(())
}

fn row(r: &Row<'_>) -> rusqlite::Result<OwnerAlert> {
    let urgency: String = r.get("urgency")?;
    let state: String = r.get("state")?;
    Ok(OwnerAlert {
        id: r.get("id")?,
        source_url: r.get("source_url")?,
        sender: r.get("sender")?,
        action: r.get("action")?,
        reason: r.get("reason")?,
        urgency: match urgency.as_str() {
            "critical" => Urgency::Critical,
            "high" => Urgency::High,
            _ => Urgency::Routine,
        },
        state: match state.as_str() {
            "resolved" => AlertState::Resolved,
            "acknowledged" => AlertState::Acknowledged,
            _ => AlertState::Open,
        },
        due_at_ms: r.get("due_at_ms")?,
        timezone: r.get("timezone")?,
        meeting_id: r.get("meeting_id")?,
        expires_at_ms: r.get("expires_at_ms")?,
        text_after_ms: r.get("text_after_ms")?,
        created_at_ms: r.get("created_at_ms")?,
        updated_at_ms: r.get("updated_at_ms")?,
        snoozed_until_ms: r.get("snoozed_until_ms")?,
    })
}

fn reconcile_texts(c: &Connection, now: i64) -> rusqlite::Result<()> {
    c.execute("UPDATE owner_alert_texts SET disposition=CASE
                WHEN (SELECT state FROM owner_alerts WHERE id=alert_id)!='open' THEN 'cancelled' ELSE 'expired' END
                WHERE disposition IS NULL AND outbox_id IN (SELECT id FROM imessage_outbox WHERE status='queued')
                AND alert_id IN (SELECT id FROM owner_alerts WHERE state!='open' OR expires_at_ms<=?1 OR due_at_ms<=?1)", [now])?;
    c.execute("UPDATE imessage_outbox SET status='failed',completed_at_ms=?1,
                error_detail=(SELECT disposition FROM owner_alert_texts WHERE outbox_id=imessage_outbox.id)
                WHERE status='queued' AND id IN (SELECT outbox_id FROM owner_alert_texts WHERE disposition IS NOT NULL)", [now])?;
    Ok(())
}

impl Store {
    pub fn reconcile_owner_alert_texts(&self, now: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            let tx =
                rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
            reconcile_texts(&tx, now)?;
            tx.commit()
        })
    }

    pub fn record_owner_text_heartbeat(&self, now: i64, error: Option<&str>) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE owner_text_sender_health SET heartbeat_at_ms=?1,error=?2
            WHERE singleton=1",
                params![now, error],
            )
        })?;
        Ok(())
    }

    pub fn owner_text_health(&self, now: i64, timeout_ms: i64) -> StoreResult<OwnerTextHealth> {
        self.with_conn(|c| {
            let (heartbeat, error): (Option<i64>,Option<String>) = c.query_row(
                "SELECT heartbeat_at_ms,error FROM owner_text_sender_health WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?)))?;
            let enabled = c.query_row("SELECT enabled FROM owner_alert_text_policy WHERE singleton=1", [], |r| r.get(0))?;
            let last_success = c.query_row("SELECT MAX(completed_at_ms) FROM imessage_outbox WHERE status='sent'", [], |r| r.get(0))?;
            let healthy = heartbeat.is_some_and(|t| now.saturating_sub(t)<timeout_ms) && error.is_none();
            let detail = error.unwrap_or_else(|| if healthy { "Sender reachable; phone receipt is not verified by this heartbeat".into() }
                else { "No sender heartbeat within two minutes. Check Mac power/sleep, Tailscale and SSH connectivity, and launchctl print gui/$(id -u)/org.augmentagent.imessage-send on the Mac".into() });
            Ok(OwnerTextHealth { enabled,healthy,detail,heartbeat_at_ms:heartbeat,last_success_at_ms:last_success })
        })
    }

    pub fn claim_owner_text_health_notice(&self, now: i64, interval_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE owner_text_sender_health SET notified_at_ms=?1
            WHERE singleton=1 AND (notified_at_ms IS NULL OR notified_at_ms<=?1-?2)",
                params![now, interval_ms],
            )
        })? == 1)
    }

    /// Separate owner opt-in, independent of approved-contact allowlists.
    pub fn configure_owner_alert_texts(
        &self,
        destination: Option<&str>,
        enabled: bool,
    ) -> StoreResult<()> {
        if enabled && destination.is_none() {
            return Err(StoreError::InvalidInput(
                "enabling owner texts requires an explicit destination".into(),
            ));
        }
        if let Some(target) = destination {
            if target.trim().is_empty()
                || target.contains(char::is_whitespace)
                || target.contains('\0')
            {
                return Err(StoreError::InvalidInput("invalid owner destination".into()));
            }
        }
        self.with_conn(|c| {
            c.execute(
                "UPDATE owner_alert_text_policy SET destination=COALESCE(?1,destination),enabled=?2
            WHERE singleton=1",
                params![destination, enabled],
            )
        })?;
        Ok(())
    }

    /// The selection, policy check and deduplication insert are one transaction.
    pub fn enqueue_owner_alert_texts(&self, now: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
            let target: Option<String> = tx.query_row("SELECT destination FROM owner_alert_text_policy WHERE enabled=1", [], |r| r.get(0)).optional()?.flatten();
            let Some(target) = target else { return Ok(0) };
            let alerts = {
                let mut stmt = tx.prepare("SELECT * FROM owner_alerts a WHERE state='open'
                    AND urgency IN ('high','critical') AND text_after_ms<=?1 AND expires_at_ms>?1
                    AND (due_at_ms IS NULL OR due_at_ms>?1)
                    AND (snoozed_until_ms IS NULL OR snoozed_until_ms<=?1)
                    AND NOT EXISTS (SELECT 1 FROM owner_alert_texts t WHERE t.alert_id=a.id)
                    ORDER BY text_after_ms,id LIMIT 100")?;
                let rows = stmt.query_map([now], row)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for a in &alerts {
                let body = a.text_summary(now);
                // Random internal action namespace cannot collide with imported source IDs.
                let action_id = format!("owner-alert:{}", uuid::Uuid::new_v4());
                tx.execute("INSERT INTO imessage_outbox(action_id,target,target_kind,service,body,created_at_ms)
                    VALUES (?1,?2,'handle','iMessage',?3,?4)",params![action_id,target,body,now])?;
                tx.execute("INSERT INTO owner_alert_texts(alert_id,outbox_id) VALUES(?1,?2)", params![a.id,tx.last_insert_rowid()])?;
            }
            tx.commit()?;
            Ok(alerts.len())
        })
    }

    /// Final policy/lifecycle check at dispatch. After this atomic claim, acknowledgment
    /// cannot retract a send already handed to the Mac; it can only cancel future work.
    pub fn claim_owner_alert_text(
        &self,
        now: i64,
    ) -> StoreResult<Option<crate::ImessageOutboxItem>> {
        self.with_conn(|c| {
            let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
            reconcile_texts(&tx, now)?;
            let sql = format!("UPDATE imessage_outbox SET status='claimed',claimed_at_ms=?1
                WHERE id=(SELECT o.id FROM imessage_outbox o
                    JOIN owner_alert_texts t ON t.outbox_id=o.id JOIN owner_alerts a ON a.id=t.alert_id
                    JOIN owner_alert_text_policy p ON p.singleton=1
                    WHERE o.status='queued' AND t.disposition IS NULL AND a.state='open'
                    AND p.enabled=1 AND p.destination=o.target AND a.expires_at_ms>?1
                    AND (a.due_at_ms IS NULL OR a.due_at_ms>?1)
                    AND (a.snoozed_until_ms IS NULL OR a.snoozed_until_ms<=?1)
                    ORDER BY a.text_after_ms,o.id LIMIT 1) AND status='queued'
                RETURNING {}", crate::imessage::COLUMNS);
            let mut item = tx.query_row(&sql, [now], crate::imessage::row_to_item).optional()?;
            if let Some(item) = item.as_mut() {
                let alert = tx.query_row("SELECT a.* FROM owner_alerts a JOIN owner_alert_texts t ON t.alert_id=a.id
                    WHERE t.outbox_id=?1", [item.id], row)?;
                item.body = alert.text_summary(now);
                tx.execute("UPDATE imessage_outbox SET body=?2 WHERE id=?1",params![item.id,item.body])?;
            }
            tx.commit()?;
            Ok(item)
        })
    }

    pub fn owner_alert_text_status(&self, id: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT COALESCE(t.disposition,o.status)
            FROM owner_alert_texts t JOIN imessage_outbox o ON o.id=t.outbox_id
            WHERE t.alert_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
        })
    }

    pub fn owner_alert_for_outbox(&self, outbox_id: i64) -> StoreResult<Option<OwnerAlert>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT a.* FROM owner_alerts a JOIN owner_alert_texts t
            ON t.alert_id=a.id WHERE t.outbox_id=?1",
                [outbox_id],
                row,
            )
            .optional()
        })
    }

    pub fn insert_owner_alert(&self, a: &NewOwnerAlert<'_>, now: i64) -> StoreResult<bool> {
        if a.timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(StoreError::InvalidInput(
                "invalid owner alert timezone".into(),
            ));
        }
        if a.id.len() > 80
            || a.sender.chars().count() > 120
            || a.action.chars().count() > 400
            || a.reason.chars().count() > 400
            || a.source_url.len() > 1500
        {
            return Err(StoreError::InvalidInput(
                "owner alert fields exceed concise notification limits".into(),
            ));
        }
        for (name, value) in [
            ("id", a.id),
            ("source URL", a.source_url),
            ("sender", a.sender),
            ("action", a.action),
            ("reason", a.reason),
            ("timezone", a.timezone),
        ] {
            if value.trim().is_empty() || value.contains('\0') {
                return Err(StoreError::InvalidInput(format!(
                    "owner alert requires {name}"
                )));
            }
        }
        if !(a.source_url.starts_with("https://") || a.source_url.starts_with("http://")) {
            return Err(StoreError::InvalidInput(
                "owner alert source must be an HTTP(S) link".into(),
            ));
        }
        Ok(self.with_conn(|c| {
            c.execute(
                "INSERT INTO owner_alerts
            (id,source_url,sender,action,reason,urgency,due_at_ms,timezone,meeting_id,
             expires_at_ms,text_after_ms,created_at_ms,updated_at_ms)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)
            ON CONFLICT(id) DO NOTHING",
                params![
                    a.id,
                    a.source_url,
                    a.sender,
                    a.action,
                    a.reason,
                    a.urgency.as_str(),
                    a.due_at_ms,
                    a.timezone,
                    a.meeting_id,
                    a.expires_at_ms,
                    if a.urgency == Urgency::Critical {
                        now
                    } else {
                        a.text_after_ms
                    },
                    now
                ],
            )
        })? == 1)
    }

    pub fn owner_alert(&self, id: &str) -> StoreResult<Option<OwnerAlert>> {
        self.with_conn(|c| {
            c.query_row("SELECT * FROM owner_alerts WHERE id=?1", [id], row)
                .optional()
        })
    }

    pub fn owner_alerts_due_for_text(&self, now: i64) -> StoreResult<Vec<OwnerAlert>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM owner_alerts WHERE state='open'
                AND urgency IN ('critical','high') AND text_after_ms<=?1 AND expires_at_ms>?1
                AND (due_at_ms IS NULL OR due_at_ms>?1)
                AND (snoozed_until_ms IS NULL OR snoozed_until_ms<=?1)
                ORDER BY text_after_ms,id",
            )?;
            let rows = stmt.query_map([now], row)?;
            rows.collect()
        })
    }

    /// Resolution is terminal. Repeated button presses preserve original timestamps.
    pub fn set_owner_alert_state(
        &self,
        id: &str,
        state: AlertState,
        now: i64,
    ) -> StoreResult<bool> {
        if state == AlertState::Open {
            return Err(StoreError::InvalidInput(
                "cannot reopen an owner alert through acknowledgment".into(),
            ));
        }
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE owner_alerts SET state=?2,updated_at_ms=?3
            WHERE id=?1 AND state!='resolved' AND state!=?2",
                params![id, state.as_str(), now],
            )
        })? == 1)
    }

    pub fn snooze_owner_alert(
        &self,
        id: &str,
        until: i64,
        override_deadline: bool,
        now: i64,
    ) -> StoreResult<bool> {
        if until <= now {
            return Err(StoreError::InvalidInput(
                "snooze must end in the future".into(),
            ));
        }
        let Some(a) = self.owner_alert(id)? else {
            return Ok(false);
        };
        if !override_deadline
            && (until >= a.expires_at_ms || a.due_at_ms.is_some_and(|due| until >= due))
        {
            return Err(StoreError::InvalidInput(
                "snooze crosses the deadline; explicit owner override required".into(),
            ));
        }
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE owner_alerts SET snoozed_until_ms=?2,updated_at_ms=?3
            WHERE id=?1 AND state!='resolved'",
                params![id, until, now],
            )
        })? == 1)
    }
}
