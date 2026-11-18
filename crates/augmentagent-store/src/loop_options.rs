//! Per-loop delivery options, keyed coverage and delivery records (#2246).
//!
//! Options live beside `user_loops` rather than in it, so a loop created
//! without any of them has no row here and behaves exactly as before.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopOptions {
    /// A complete run with no findings posts nothing.
    #[serde(default)]
    pub quiet_when_empty: bool,
    /// A keyed finding may be reported again once this long has passed since
    /// its last report. `None` reports each key once.
    #[serde(default)]
    pub rereport_after_secs: Option<i64>,
    /// Wiki-relative file read at every run and appended to the prompt.
    #[serde(default)]
    pub prompt_file: Option<String>,
    /// Owner address each new finding is emailed to.
    #[serde(default)]
    pub email_to: Option<String>,
    /// Connected account that sends those emails. `None` uses the only one.
    #[serde(default)]
    pub email_account: Option<String>,
}

impl LoopOptions {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

const DELIVERY_MARKER: &str = "loop-delivery:";

/// Footer line stamped on every email a loop sends. The token is random and
/// recorded at send time, which is what lets the inbound side recognise it.
pub fn loop_delivery_footer(loop_id: &str, token: &str) -> String {
    format!("(Sent by scheduled loop {loop_id}. Ref {DELIVERY_MARKER}{token})")
}

fn delivery_token(body: &str) -> Option<&str> {
    let start = body.rfind(DELIVERY_MARKER)? + DELIVERY_MARKER.len();
    let token = body.get(start..start + 36)?;
    token
        .bytes()
        .all(|b| b.is_ascii_hexdigit() || b == b'-')
        .then_some(token)
}

fn bare_address(header: &str) -> &str {
    match (header.rfind('<'), header.rfind('>')) {
        (Some(open), Some(close)) if open < close => &header[open + 1..close],
        _ => header,
    }
}

pub(super) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS loop_options (
        loop_id TEXT PRIMARY KEY, quiet_when_empty INTEGER NOT NULL DEFAULT 0,
        rereport_after_secs INTEGER, prompt_file TEXT, email_to TEXT, email_account TEXT);
        CREATE TABLE IF NOT EXISTS loop_deliveries (
        token TEXT PRIMARY KEY, loop_id TEXT NOT NULL, from_addr TEXT NOT NULL,
        to_addr TEXT NOT NULL, subject TEXT NOT NULL, sent_at_ms INTEGER NOT NULL);",
    )?;
    if !column_exists(conn, "loop_coverage", "key")? {
        conn.execute("ALTER TABLE loop_coverage ADD COLUMN key TEXT", [])?;
    }
    Ok(())
}

/// Write one coverage row. A keyed finding replaces the earlier report of the
/// same key, so the re-report window is measured from the latest report. An
/// unkeyed finding keeps its first report, as before.
pub(super) fn insert_coverage(
    conn: &Connection,
    loop_id: &str,
    request_id: &str,
    finding: &LoopFinding,
    now: i64,
) -> StoreResult<()> {
    if finding.fingerprint.is_empty()
        || finding.finding.trim().is_empty()
        || finding.evidence.is_empty()
        || finding.evidence.iter().any(|e| e.trim().is_empty())
    {
        return Err(StoreError::InvalidInput(
            "coverage requires concrete findings and evidence".into(),
        ));
    }
    let evidence = serde_json::to_string(&finding.evidence)
        .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
    let conflict = if finding.key.is_some() {
        "ON CONFLICT(loop_id,fingerprint) DO UPDATE SET finding=excluded.finding,\
         evidence=excluded.evidence,request_id=excluded.request_id,\
         completed_at_ms=excluded.completed_at_ms,key=excluded.key"
    } else {
        "ON CONFLICT(loop_id,fingerprint) DO NOTHING"
    };
    conn.execute(
        &format!(
            "INSERT INTO loop_coverage(loop_id,fingerprint,finding,evidence,request_id,completed_at_ms,key) \
             VALUES (?1,?2,?3,?4,?5,?6,?7) {conflict}"
        ),
        params![
            loop_id,
            finding.fingerprint,
            redact::mask(&finding.finding),
            redact::mask(&evidence),
            request_id,
            now,
            finding.key
        ],
    )?;
    Ok(())
}

impl Store {
    /// Store a loop's options. Default options remove the row.
    pub fn set_loop_options(&self, loop_id: &str, options: &LoopOptions) -> StoreResult<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        if options.is_default() {
            conn.execute("DELETE FROM loop_options WHERE loop_id=?1", params![loop_id])?;
            return Ok(());
        }
        if options.rereport_after_secs.is_some_and(|secs| secs <= 0) {
            return Err(StoreError::InvalidInput(
                "loop re-report window must be positive".into(),
            ));
        }
        conn.execute(
            "INSERT OR REPLACE INTO loop_options VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                loop_id,
                options.quiet_when_empty,
                options.rereport_after_secs,
                options.prompt_file,
                options.email_to,
                options.email_account
            ],
        )?;
        Ok(())
    }

    /// A loop's options; the defaults when it has none.
    pub fn loop_options(&self, loop_id: &str) -> StoreResult<LoopOptions> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT quiet_when_empty,rereport_after_secs,prompt_file,email_to,email_account \
                 FROM loop_options WHERE loop_id=?1",
                params![loop_id],
                |r| {
                    Ok(LoopOptions {
                        quiet_when_empty: r.get::<_, i64>(0)? != 0,
                        rereport_after_secs: r.get(1)?,
                        prompt_file: r.get(2)?,
                        email_to: r.get(3)?,
                        email_account: r.get(4)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    /// When this finding was last reported for the loop, if ever.
    pub fn loop_finding_reported_at(
        &self,
        loop_id: &str,
        fingerprint: &str,
    ) -> StoreResult<Option<i64>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT completed_at_ms FROM loop_coverage WHERE loop_id=?1 AND fingerprint=?2",
                params![loop_id, fingerprint],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Record one delivered finding before the occurrence completes, so a
    /// retry after a partial delivery does not deliver it twice.
    pub fn record_loop_finding(
        &self,
        loop_id: &str,
        request_id: &str,
        finding: &LoopFinding,
    ) -> StoreResult<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        insert_coverage(&conn, loop_id, request_id, finding, now_millis())
    }

    /// Keys reported at or after `since_ms` (all keys when `None`), newest first.
    pub fn loop_covered_keys(
        &self,
        loop_id: &str,
        since_ms: Option<i64>,
        limit: usize,
    ) -> StoreResult<Vec<String>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT key FROM loop_coverage WHERE loop_id=?1 AND key IS NOT NULL \
             AND completed_at_ms>=?2 ORDER BY completed_at_ms DESC,key LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![loop_id, since_ms.unwrap_or(i64::MIN), limit as i64],
                |r| r.get(0),
            )?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// Remember an email this daemon sent for a loop, so the copy that
    /// arrives in a connected inbox can be recognised and left alone.
    pub fn record_loop_delivery(
        &self,
        token: &str,
        loop_id: &str,
        from_addr: &str,
        to_addr: &str,
        subject: &str,
    ) -> StoreResult<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO loop_deliveries VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                token,
                loop_id,
                from_addr.trim().to_ascii_lowercase(),
                to_addr.trim().to_ascii_lowercase(),
                subject.trim(),
                now_millis()
            ],
        )?;
        Ok(())
    }

    /// Whether an inbound email is one a loop sent: its body carries a
    /// recorded token, and its sender and subject are the ones recorded.
    pub fn is_loop_delivery_email(&self, from: &str, subject: &str, body: &str) -> StoreResult<bool> {
        match delivery_token(body) {
            Some(token) => self.is_loop_delivery(token, bare_address(from), subject),
            None => Ok(false),
        }
    }

    /// True only for the exact email this daemon sent: same token, sender and
    /// subject. A reply or forward that quotes the token does not match.
    pub fn is_loop_delivery(&self, token: &str, from_addr: &str, subject: &str) -> StoreResult<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM loop_deliveries WHERE token=?1 AND from_addr=?2 AND subject=?3)",
            params![token, from_addr.trim().to_ascii_lowercase(), subject.trim()],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        (dir, store)
    }
    fn finding(fingerprint: &str, key: Option<&str>, text: &str) -> LoopFinding {
        LoopFinding {
            fingerprint: fingerprint.into(),
            finding: text.into(),
            evidence: vec!["fixture:v1".into()],
            key: key.map(Into::into),
            body: None,
        }
    }

    #[test]
    fn a_loop_without_options_reads_the_defaults_and_stores_no_row() {
        let (_dir, store) = store();
        assert!(store.loop_options("loop").unwrap().is_default());
        store.set_loop_options("loop", &LoopOptions::default()).unwrap();
        let rows: i64 = store
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM loop_options", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn options_round_trip_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let options = LoopOptions {
            quiet_when_empty: true,
            rereport_after_secs: Some(86_400),
            prompt_file: Some("scans/example.md".into()),
            email_to: Some("owner@example.com".into()),
            email_account: Some("sender@example.com".into()),
        };
        Store::open(&path).unwrap().set_loop_options("loop", &options).unwrap();
        assert_eq!(Store::open(&path).unwrap().loop_options("loop").unwrap(), options);
    }

    #[test]
    fn a_non_positive_rereport_window_is_refused() {
        let (_dir, store) = store();
        let options = LoopOptions { rereport_after_secs: Some(0), ..Default::default() };
        assert!(store.set_loop_options("loop", &options).is_err());
    }

    #[test]
    fn a_keyed_finding_replaces_its_earlier_report_and_an_unkeyed_one_keeps_it() {
        let (_dir, store) = store();
        store.record_loop_finding("loop", "r1", &finding("k", Some("person-a"), "first")).unwrap();
        store.record_loop_finding("loop", "r1", &finding("u", None, "first")).unwrap();
        let first = store.loop_finding_reported_at("loop", "k").unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.record_loop_finding("loop", "r2", &finding("k", Some("person-a"), "second")).unwrap();
        store.record_loop_finding("loop", "r2", &finding("u", None, "second")).unwrap();
        assert!(store.loop_finding_reported_at("loop", "k").unwrap().unwrap() > first);
        let rows = store.loop_coverage("loop", 10).unwrap();
        assert_eq!(rows.len(), 2);
        let keyed = rows.iter().find(|f| f.fingerprint == "k").unwrap();
        assert_eq!((keyed.finding.as_str(), keyed.key.as_deref()), ("second", Some("person-a")));
        assert_eq!(rows.iter().find(|f| f.fingerprint == "u").unwrap().finding, "first");
        assert_eq!(store.loop_finding_reported_at("other", "k").unwrap(), None);
    }

    #[test]
    fn covered_keys_respect_the_window_and_the_loop() {
        let (_dir, store) = store();
        store.record_loop_finding("loop", "r1", &finding("a", Some("person-a"), "x")).unwrap();
        store.record_loop_finding("loop", "r1", &finding("u", None, "x")).unwrap();
        store.record_loop_finding("other", "r1", &finding("b", Some("person-b"), "x")).unwrap();
        assert_eq!(store.loop_covered_keys("loop", None, 10).unwrap(), vec!["person-a"]);
        assert!(store.loop_covered_keys("loop", Some(now_millis() + 60_000), 10).unwrap().is_empty());
    }

    #[test]
    fn coverage_without_evidence_is_refused() {
        let (_dir, store) = store();
        let mut bad = finding("k", Some("person-a"), "x");
        bad.evidence.clear();
        assert!(store.record_loop_finding("loop", "r1", &bad).is_err());
    }

    #[test]
    fn only_the_exact_sent_email_is_a_loop_delivery() {
        let (_dir, store) = store();
        store
            .record_loop_delivery("token-1", "loop", "Sender@Example.com", "owner@example.com", "Lead: Example Person")
            .unwrap();
        assert!(store.is_loop_delivery("token-1", "sender@example.com", "Lead: Example Person").unwrap());
        assert!(!store.is_loop_delivery("token-1", "sender@example.com", "Re: Lead: Example Person").unwrap());
        assert!(!store.is_loop_delivery("token-1", "stranger@example.com", "Lead: Example Person").unwrap());
        assert!(!store.is_loop_delivery("token-2", "sender@example.com", "Lead: Example Person").unwrap());
    }

    #[test]
    fn an_inbound_copy_of_a_loop_email_is_recognised_by_its_footer() {
        let (_dir, store) = store();
        let token = "0f8fad5b-d9cb-469f-a165-70867728950e";
        store
            .record_loop_delivery(token, "loop", "sender@example.com", "owner@example.com", "Lead: Example Person")
            .unwrap();
        let body = format!("Details here.\n\n{}", loop_delivery_footer("loop", token));
        let from = "Example Sender <sender@example.com>";
        assert!(store.is_loop_delivery_email(from, "Lead: Example Person", &body).unwrap());
        assert!(!store.is_loop_delivery_email(from, "Re: Lead: Example Person", &body).unwrap());
        assert!(!store.is_loop_delivery_email("Other <other@example.com>", "Lead: Example Person", &body).unwrap());
        assert!(!store.is_loop_delivery_email(from, "Lead: Example Person", "Details here.").unwrap());
        let forged = body.replace(token, "11111111-2222-3333-4444-555555555555");
        assert!(!store.is_loop_delivery_email(from, "Lead: Example Person", &forged).unwrap());
        assert!(!store.is_loop_delivery_email(from, "Lead: Example Person", "loop-delivery:short").unwrap());
    }

    #[test]
    fn a_legacy_coverage_table_gains_the_key_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE loop_coverage (
                loop_id TEXT NOT NULL, fingerprint TEXT NOT NULL, finding TEXT NOT NULL,
                evidence TEXT NOT NULL, request_id TEXT NOT NULL, completed_at_ms INTEGER NOT NULL,
                PRIMARY KEY(loop_id,fingerprint));
                INSERT INTO loop_coverage VALUES ('loop','old','legacy','[\"fixture:v0\"]','r0',1);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let rows = store.loop_coverage("loop", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, None);
        store.record_loop_finding("loop", "r1", &finding("new", Some("person-a"), "x")).unwrap();
        assert_eq!(store.loop_covered_keys("loop", None, 10).unwrap(), vec!["person-a"]);
    }
}
