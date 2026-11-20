//! One durable, bounded high-alert text per local schedule slot, and the opt-in
//! Discord digest of one preceding local date (#2329). Unknown delivery
//! is never retried automatically. All timestamps are supplied by the caller.
use crate::{
    owner_alerts::{row, OwnerAlert},
    ImessageOutboxItem, Store, StoreError, StoreResult,
};
use chrono::{TimeZone, Timelike};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

pub(crate) fn migrate(c: &Connection) -> StoreResult<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner_alert_digests (
        slot TEXT PRIMARY KEY, outbox_id INTEGER NOT NULL UNIQUE,
        expires_at_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS owner_alert_digest_items (
        slot TEXT NOT NULL, alert_id TEXT NOT NULL, PRIMARY KEY(slot,alert_id));
        CREATE TABLE IF NOT EXISTS owner_alert_day_digests (
        zone TEXT NOT NULL, local_date TEXT NOT NULL, claimed_at_ms INTEGER NOT NULL,
        PRIMARY KEY(zone,local_date));",
    )?;
    Ok(())
}
/// Strict HH:MM pairs, checked even outside a scheduled minute.
pub fn schedule(value: &str) -> StoreResult<Vec<String>> {
    let mut slots = Vec::new();
    for s in value.split(',').map(str::trim) {
        let valid = s.is_ascii()
            && s.len() == 5
            && s.as_bytes()[2] == b':'
            && s[..2].parse::<u8>().is_ok_and(|v| v < 24)
            && s[3..].parse::<u8>().is_ok_and(|v| v < 60);
        if !s.is_ascii() || !valid {
            return Err(StoreError::InvalidInput(
                "digest schedule must contain HH:MM local times".into(),
            ));
        }
        if !slots.contains(&s.to_string()) {
            slots.push(s.to_string());
        }
    }
    Ok(slots)
}
fn eligible(c: &Connection, now: i64, slot: Option<&str>) -> rusqlite::Result<Vec<OwnerAlert>> {
    let mut q=c.prepare("SELECT a.* FROM owner_alerts a
        WHERE a.state='open' AND a.urgency='high' AND a.expires_at_ms>?1
        AND (a.due_at_ms IS NULL OR a.due_at_ms>?1)
        AND (a.snoozed_until_ms IS NULL OR a.snoozed_until_ms<=?1)
        AND (?2 IS NULL OR EXISTS(SELECT 1 FROM owner_alert_digest_items d WHERE d.alert_id=a.id AND d.slot=?2))
        AND NOT EXISTS(SELECT 1 FROM owner_alert_texts t JOIN imessage_outbox o ON o.id=t.outbox_id
            WHERE t.alert_id=a.id AND o.status IN ('sent','claimed','unknown'))
        ORDER BY a.created_at_ms,a.id LIMIT 20")?;
    let rows = q.query_map(params![now, slot], row)?;
    rows.collect()
}
fn body(alerts: &[OwnerAlert]) -> String {
    let mut text = String::from("Open HIGH alerts (not individually texted):");
    for a in alerts {
        text.push_str(&format!(
            "\n• {}: {}\n{}",
            a.sender.chars().take(80).collect::<String>(),
            a.action.chars().take(200).collect::<String>(),
            a.source_url.chars().take(300).collect::<String>()
        ));
    }
    text
}
impl Store {
    pub fn enqueue_owner_alert_digest(
        &self,
        now: i64,
        timezone: &str,
        times: &str,
    ) -> StoreResult<usize> {
        let slots = schedule(times)?;
        let tz: chrono_tz::Tz = timezone
            .parse()
            .map_err(|_| StoreError::InvalidInput("invalid digest timezone".into()))?;
        let local = chrono::DateTime::from_timestamp_millis(now)
            .ok_or_else(|| StoreError::InvalidInput("invalid digest timestamp".into()))?
            .with_timezone(&tz);
        let hhmm = local.format("%H:%M").to_string();
        if !slots.contains(&hhmm) {
            return Ok(0);
        }
        // Local date+time deduplicates the repeated minute at a DST fall-back.
        let slot = format!("{timezone}:{}:{hhmm}", local.format("%Y-%m-%d"));
        let expires =
            now - i64::from(local.second()) * 1000 - i64::from(local.nanosecond() / 1_000_000)
                + 300_000;
        self.with_conn(|c| {
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let target:Option<String>=tx.query_row("SELECT destination FROM owner_alert_text_policy WHERE enabled=1",[],|r|r.get(0)).optional()?.flatten();
            let Some(target)=target else{return Ok(0)};
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM owner_alert_digests WHERE slot=?1)",[&slot],|r|r.get::<_,bool>(0))? {return Ok(0)}
            let alerts=eligible(&tx,now,None)?;
            if alerts.is_empty(){return Ok(0)}
            tx.execute("INSERT INTO imessage_outbox(action_id,target,target_kind,service,body,created_at_ms)
                VALUES(?1,?2,'handle','iMessage',?3,?4)",params![format!("owner-digest:{}",uuid::Uuid::new_v4()),target,body(&alerts),now])?;
            tx.execute("INSERT INTO owner_alert_digests(slot,outbox_id,expires_at_ms) VALUES(?1,?2,?3)",params![slot,tx.last_insert_rowid(),expires])?;
            for a in alerts {tx.execute("INSERT INTO owner_alert_digest_items(slot,alert_id) VALUES(?1,?2)",params![slot,a.id])?;}
            tx.commit()?; Ok(1)
        })
    }
    pub fn claim_owner_alert_digest(&self, now: i64) -> StoreResult<Option<ImessageOutboxItem>> {
        self.with_conn(|c| {
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            tx.execute("UPDATE imessage_outbox SET status='failed',completed_at_ms=?1,error_detail='digest expired'
                WHERE status='queued' AND id IN(SELECT outbox_id FROM owner_alert_digests WHERE expires_at_ms<=?1)",[now])?;
            let queued={let mut q=tx.prepare("SELECT d.slot,o.id FROM owner_alert_digests d JOIN imessage_outbox o ON o.id=d.outbox_id
                JOIN owner_alert_text_policy p ON p.singleton=1 WHERE o.status='queued' AND p.enabled=1 AND p.destination=o.target ORDER BY o.id")?;
                let rows=q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?; rows.collect::<Result<Vec<_>,_>>()?};
            for (slot,id) in queued {
                let alerts=eligible(&tx,now,Some(&slot))?;
                if alerts.is_empty(){tx.execute("UPDATE imessage_outbox SET status='failed',completed_at_ms=?1,error_detail='digest empty' WHERE id=?2",params![now,id])?; continue}
                let sql=format!("UPDATE imessage_outbox SET status='claimed',claimed_at_ms=?1,body=?2 WHERE id=?3 AND status='queued' RETURNING {}",crate::imessage::COLUMNS);
                let item=tx.query_row(&sql,params![now,body(&alerts),id],crate::imessage::row_to_item)?;
                tx.commit()?;return Ok(Some(item))
            }
            tx.commit()?;Ok(None)
        })
    }
    pub fn owner_alert_digest_deadline(&self, id: i64) -> StoreResult<Option<i64>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT expires_at_ms FROM owner_alert_digests WHERE outbox_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
        })
    }
}

/// One preceding local date, summarised for the opt-in Discord digest (#2329):
/// counts plus the subjects of the high-urgency alerts handled on Discord that
/// never texted. Message content — bodies, senders, handles — is absent by
/// construction, because a quiet phone is the thing being explained.
#[derive(Debug, Clone)]
pub struct AlertDayDigest {
    pub zone: String,
    pub local_date: String,
    pub alerts: i64,
    pub high: i64,
    pub texted: i64,
    pub subjects: Vec<String>,
}

/// Local hours in which the digest for the preceding date may be delivered.
const MORNING: std::ops::Range<u32> = 8..12;
/// A digest is a glance, not an inbox.
const MAX_SUBJECTS: usize = 10;
/// An individual text was claimed, sent, or left unknown.
const TEXTED: &str = "EXISTS(SELECT 1 FROM owner_alert_texts t JOIN imessage_outbox o ON o.id=t.outbox_id
    WHERE t.alert_id=a.id AND o.status IN ('sent','claimed','unknown'))";

fn local_midnight(tz: chrono_tz::Tz, date: chrono::NaiveDate) -> StoreResult<i64> {
    let naive = date.and_hms_opt(0, 0, 0).expect("midnight exists on every date");
    // A spring-forward midnight does not exist locally; the hour after it does.
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.timestamp_millis())
        .ok_or_else(|| StoreError::InvalidInput("unresolvable local digest date".into()))
}

impl Store {
    /// Claim the delivery key for the preceding local date and return its
    /// summary in the same immediate transaction, so a restart after the claim
    /// cannot post the date twice. `None` means there is nothing to deliver:
    /// outside the morning window, already claimed, or a date without alerts.
    /// The zone is the one persisted with the most recent alert — alert data is
    /// the only configured source of the owner's local day.
    pub fn claim_owner_alert_day_digest(&self, now: i64) -> StoreResult<Option<AlertDayDigest>> {
        let zone: Option<String> = self.with_conn(|c| {
            c.query_row(
                "SELECT timezone FROM owner_alerts ORDER BY created_at_ms DESC,id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
        })?;
        let Some(zone) = zone else { return Ok(None) };
        let tz: chrono_tz::Tz = zone
            .parse()
            .map_err(|_| StoreError::InvalidInput("invalid owner alert timezone".into()))?;
        let local = chrono::DateTime::from_timestamp_millis(now)
            .ok_or_else(|| StoreError::InvalidInput("invalid digest timestamp".into()))?
            .with_timezone(&tz);
        if !MORNING.contains(&local.hour()) {
            return Ok(None);
        }
        let date = local
            .date_naive()
            .pred_opt()
            .ok_or_else(|| StoreError::InvalidInput("invalid digest date".into()))?;
        // Timezone-aware bounds: a DST date is 23 or 25 hours long, never 24.
        let start = local_midnight(tz, date)?;
        let end = local_midnight(
            tz,
            date.succ_opt()
                .ok_or_else(|| StoreError::InvalidInput("invalid digest date".into()))?,
        )?;
        let key = date.to_string();
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let (alerts, high, texted): (i64, i64, i64) = tx.query_row(
                &format!("SELECT COUNT(*),COALESCE(SUM(a.urgency='high'),0),COALESCE(SUM({TEXTED}),0)
                    FROM owner_alerts a WHERE a.created_at_ms>=?1 AND a.created_at_ms<?2"),
                params![start, end],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if alerts == 0 {
                tx.commit()?;
                return Ok(None);
            }
            if tx.execute("INSERT OR IGNORE INTO owner_alert_day_digests(zone,local_date,claimed_at_ms)
                VALUES(?1,?2,?3)",params![zone,key,now])? == 0 {
                tx.commit()?;
                return Ok(None);
            }
            let subjects = {
                let mut q = tx.prepare(&format!(
                    "SELECT json_extract(d.payload,'$.subject') FROM owner_alerts a
                    JOIN owner_alert_details d ON d.alert_id=a.id
                    WHERE a.created_at_ms>=?1 AND a.created_at_ms<?2 AND a.urgency='high'
                    AND a.state IN ('acknowledged','resolved')
                    AND EXISTS(SELECT 1 FROM owner_alert_notices n WHERE n.alert_id=a.id AND n.status='posted')
                    AND NOT {TEXTED} ORDER BY a.created_at_ms,a.id LIMIT ?3"
                ))?;
                let rows = q.query_map(params![start, end, MAX_SUBJECTS], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<Option<String>>>>()?
            };
            tx.commit()?;
            Ok(Some(AlertDayDigest {
                zone,
                local_date: key,
                alerts,
                high,
                texted,
                subjects: subjects.into_iter().flatten().collect(),
            }))
        })
    }
}
